//! One `git worktree` per agent, so several agents can edit one repository at once.
//!
//! This is OpenHands' mechanic and feature 4's whole content: without it, two coding agents
//! pointed at the same checkout write over each other's edits and neither of them is wrong.
//! With it, each gets its own directory and its own branch, and the user merges what they
//! want afterwards.
//!
//! # Shelling out to `git`, deliberately
//!
//! There is no git library here, and adding one would be a mistake rather than a
//! convenience. `git` on the user's `PATH` is the git the user has configured: their
//! `core.hooksPath`, their credential helper, their `includeIf` conditional configs, their
//! LFS filters. A library reimplements the object store and none of that. It would also be
//! the largest dependency in the workspace, for a feature that runs four commands.
//!
//! Arguments never go through a shell — [`Command`] passes an `argv` — so a repository path
//! with a space, a quote or a newline in it needs no quoting and cannot be reinterpreted.
//! A test builds its repository under a directory with a space in the name to hold that.
//!
//! # Off unless asked
//!
//! Nothing here runs until [`crate::AgentModel::worktree`] is true, which is set from the
//! project's own toggle when a node is created. Merely having this module costs a board
//! nothing: no process is spawned, no directory is created, `git` is not even probed for.
//!
//! # The removal rule, which is RULE ZERO's spirit pointed at the user's code
//!
//! **A worktree with uncommitted work is never removed.** [`remove`] checks first and
//! returns a refusal naming the files that would have been lost. `--force` is not passed
//! anywhere in this module and must not be added: the boards are not the only thing in a
//! repository that cannot be re-created, and an agent's uncommitted work is *by definition*
//! work nobody has a copy of.
//!
//! **Removal never deletes the branch either.** `velm/<node>` is where an agent's committed
//! work lives; deleting it on removal would turn "tidy up the directory" into "throw away
//! the commits". Someone will eventually read `remove` and think the leftover branch is a
//! bug — it is the feature, and this paragraph is why.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{AgentError, Result};

/// The prefix every branch this module creates carries.
///
/// Namespaced so `git branch` tells the user at a glance which branches are Velm's, and so a
/// bulk cleanup is one glob. Stable: it is written into the user's repository.
pub const BRANCH_PREFIX: &str = "velm/";

/// The subdirectory of the app's state directory that holds worktrees.
///
/// Worktrees live outside the repository on purpose. Putting them *inside* it would make
/// every agent's checkout appear in every other agent's `git status` as an untracked
/// directory, and would put an agent's build output inside the tree it is editing.
pub const WORKTREE_DIR: &str = "wt";

/// One worktree, as git reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The checkout directory.
    pub path: PathBuf,
    /// The branch checked out there, without `refs/heads/`. `None` for a detached HEAD,
    /// which is a state a user can put a worktree into and which this module never creates.
    pub branch: Option<String>,
}

impl Worktree {
    /// Whether this is one of ours — i.e. whether removing it is this module's business.
    ///
    /// Judged by the branch prefix rather than by the directory, because a user may have
    /// moved the state directory and the branch name is the durable half.
    pub fn is_velm(&self) -> bool {
        self.branch.as_deref().is_some_and(|branch| branch.starts_with(BRANCH_PREFIX))
    }
}

/// Whether `git` can be run at all.
///
/// Probed rather than assumed: git ships with the Xcode command line tools on macOS and is
/// an optional install on Windows, so "the user has git" is a guess. A caller that skips
/// this gets [`AgentError::MissingCommand`] from the first real call instead, which is the
/// same information later; this exists so the *interface* can grey the toggle out and say
/// why rather than offering a switch that fails when flipped.
pub fn available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|out| out.status.success())
}

/// Whether `dir` is inside a git working tree.
///
/// A bare repository answers `false`, correctly: there is nothing to check out from a
/// directory with no working tree, and `git worktree add` against one behaves differently
/// enough that pretending otherwise would produce a confusing failure two steps later.
pub fn is_repo(dir: &Path) -> Result<bool> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(["rev-parse", "--is-inside-work-tree"]);
    match capture(command)? {
        Some(stdout) => Ok(stdout.trim() == "true"),
        None => Ok(false),
    }
}

/// The top of the working tree containing `dir`.
///
/// Useful because a board's project root may be any directory inside the repository, and
/// every command here wants the same anchor.
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]);
    let stdout = run(command, "git rev-parse")?;
    Ok(PathBuf::from(stdout.trim()))
}

/// Whether the repository has any commit at all.
///
/// Checked **before** [`create`] rather than by reading `git worktree add`'s stderr,
/// because an unborn HEAD is the one failure here with a remedy the user can act on and a
/// message worth writing ourselves. It is also the state a brand-new project is in, so it
/// is not an edge case — it is the first thing that happens to anybody who tries this on a
/// repository they just made.
///
/// A **detached HEAD** answers `true` and is deliberately not special-cased: `git worktree
/// add -b` branches from whatever HEAD names, which is exactly right, and refusing would
/// stop an agent working on a repository the user happens to have left on a tag.
pub fn has_commits(repo: &Path) -> Result<bool> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["rev-parse", "--verify", "--quiet", "HEAD"]);
    Ok(capture(command)?.is_some())
}

/// Where a node's worktree goes.
///
/// Exposed so a caller can show the path before committing to creating it, and so the
/// answer is derived in exactly one place — a second copy of this join is a directory the
/// remover cannot find.
pub fn path_for(state_dir: &Path, node: &str) -> PathBuf {
    state_dir.join(WORKTREE_DIR).join(slug(node))
}

/// The branch name for a node.
pub fn branch_for(node: &str) -> String {
    format!("{BRANCH_PREFIX}{}", slug(node))
}

/// A node id reduced to something safe as both a path component and a git ref.
///
/// Item ids arrive as Loro `TreeID` strings — `42@7` — which is already nearly fine, but
/// this is the one place in the module where a value from the document becomes a *path*,
/// and a `/` or a `..` in one would walk out of the state directory. So the mapping is a
/// whitelist rather than a blocklist: anything outside `[A-Za-z0-9_-]` becomes `-`.
///
/// A leading `-` is stripped as well, because `git worktree add -b -x` parses `-x` as a
/// flag rather than a branch name, and an empty result becomes `agent` so the path always
/// has a final component.
///
/// **Two different ids can collapse to one slug** — `4@7` and `4#7` both give `4-7`. That
/// is safe rather than merely unlikely: [`create`] refuses a directory that already exists,
/// so a collision surfaces as a named refusal instead of two agents silently sharing a
/// checkout. Real ids are `counter@peer`, where only the `@` is ever substituted.
pub fn slug(node: &str) -> String {
    let mut out = String::with_capacity(node.len());
    for ch in node.chars().take(64) {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_start_matches('-').to_owned();
    if trimmed.is_empty() { "agent".to_owned() } else { trimmed }
}

/// Creates a worktree for `node`, on its own branch.
///
/// Idempotent in the way that matters: if the directory is already a registered worktree,
/// that worktree is returned rather than an error. Re-opening a board must not fail because
/// the agent's checkout is exactly where it was left.
///
/// The branch is reused when it already exists — after a removal, say, which deliberately
/// leaves the branch behind — because `-b` on an existing name fails and dropping the
/// branch to make room would discard the commits it points at.
pub fn create(repo: &Path, state_dir: &Path, node: &str) -> Result<Worktree> {
    if !is_repo(repo)? {
        return Err(AgentError::Refused(format!(
            "{} is not a git repository, so there is nothing to make a worktree from",
            repo.display()
        )));
    }
    if !has_commits(repo)? {
        return Err(AgentError::Refused(format!(
            "{} has no commits yet, and git cannot add a worktree to a repository with an \
             unborn HEAD. Make one commit first.",
            repo.display()
        )));
    }

    let path = path_for(state_dir, node);
    let branch = branch_for(node);

    if path.exists() {
        // Already ours? Then this is a re-open, not a collision.
        if let Some(existing) = list(repo)?.into_iter().find(|wt| same_path(&wt.path, &path)) {
            return Ok(existing);
        }
        return Err(AgentError::Refused(format!(
            "{} already exists and is not a git worktree. Move it aside, or give this agent \
             a different working directory.",
            path.display()
        )));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentError::file(parent.display().to_string(), &error))?;
    }

    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "add"]);
    if branch_exists(repo, &branch)? {
        command.arg(&path).arg(&branch);
    } else {
        command.args(["-b", &branch]).arg(&path);
    }
    run(command, "git worktree add")?;

    Ok(Worktree { path, branch: Some(branch) })
}

/// Every worktree git knows about, the main checkout included.
///
/// Parsed from `--porcelain`, which is the stable machine-readable form; the human output
/// aligns columns and abbreviates, and would break the first time a path was long.
pub fn list(repo: &Path) -> Result<Vec<Worktree>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "list", "--porcelain"]);
    let stdout = run(command, "git worktree list")?;
    Ok(parse_list(&stdout))
}

/// The uncommitted changes in a worktree, as `git status --porcelain` lines.
///
/// **Untracked files count.** An agent's work is usually files that did not exist before, so
/// a check that only looked at tracked changes would report a directory full of new source
/// as clean and let [`remove`] delete it. A/B'd rather than assumed: with `git diff
/// --name-only` in place of `git status --porcelain`, a worktree holding a freshly written
/// `agent-notes.md` reports **`[]`** and the removal goes through.
///
/// Ignored files do *not* count, which is the user's own `.gitignore` saying that `target/`
/// is disposable — and it is the difference between this check being useful and it refusing
/// every removal forever.
pub fn dirty(worktree: &Path) -> Result<Vec<String>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(worktree).args(["status", "--porcelain"]);
    let stdout = run(command, "git status")?;
    Ok(stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        // Porcelain v1 is two status characters and a space, then the path. Those three
        // bytes are always ASCII, so `get(3..)` cannot split a character; a short line
        // falls back to itself rather than being dropped.
        .map(|line| line.get(3..).unwrap_or(line).trim().to_owned())
        .collect())
}

/// Removes a worktree, refusing while it holds work that is not committed.
///
/// `--force` is never passed. See the module header: the refusal is the feature, and the
/// message names what would have been lost so the user can commit it rather than guess.
///
/// The branch survives. That is where an agent's committed work is, and this function's job
/// is to reclaim a directory, not to discard history.
pub fn remove(repo: &Path, worktree: &Path) -> Result<()> {
    let outstanding = dirty(worktree)?;
    if !outstanding.is_empty() {
        return Err(AgentError::Refused(format!(
            "{} has uncommitted work and was not removed: {}. Commit or discard it first.",
            worktree.display(),
            summarise(&outstanding)
        )));
    }

    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "remove"]).arg(worktree);
    run(command, "git worktree remove")?;
    Ok(())
}

/// Drops git's record of worktrees whose directories are already gone.
///
/// The recovery path for a state directory that was deleted by hand — without it, git keeps
/// refusing to create `wt/42-7` because it believes one is already registered there. It
/// deletes nothing that exists; `git worktree prune` only touches metadata for directories
/// that have already vanished.
pub fn prune(repo: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(["worktree", "prune"]);
    run(command, "git worktree prune")?;
    Ok(())
}

/// Whether a local branch of that name exists.
fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]);
    Ok(capture(command)?.is_some())
}

/// Runs a git command, returning its stdout or an error carrying **git's own stderr**.
///
/// The stderr is the whole point. Git's failures are specific — *"fatal: 'velm/x' is already
/// checked out at …"* — and swallowing them leaves the user with a refusal they cannot act
/// on. A git error the user cannot see is a git error they cannot fix.
fn run(mut command: Command, what: &str) -> Result<String> {
    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AgentError::MissingCommand { command: "git".to_owned() });
        }
        Err(error) => return Err(AgentError::Io(error)),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(AgentError::Refused(if stderr.is_empty() {
            format!("`{what}` failed")
        } else {
            format!("`{what}` failed: {stderr}")
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs a git command whose *failure is an answer*, not an error.
///
/// `Ok(None)` means git ran and said no — the ref does not exist, this is not a repository.
/// An error is reserved for git not being installed at all, which is a different problem
/// with a different remedy.
fn capture(mut command: Command) -> Result<Option<String>> {
    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AgentError::MissingCommand { command: "git".to_owned() });
        }
        Err(error) => return Err(AgentError::Io(error)),
    };
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

/// Parses `git worktree list --porcelain`.
///
/// Records are separated by a blank line; `worktree <path>` opens one and `branch <ref>` or
/// `detached` says what is checked out. Pulled out as a free function so the parser is
/// testable without a repository — the format is git's and it does not change, but a typo in
/// a `strip_prefix` would otherwise only show up as an empty list at runtime.
fn parse_list(porcelain: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(previous) = current.take() {
                out.push(previous);
            }
            current = Some(Worktree { path: PathBuf::from(path.trim()), branch: None });
        } else if let Some(reference) = line.strip_prefix("branch ")
            && let Some(entry) = current.as_mut()
        {
            let name = reference.trim();
            entry.branch = Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_owned());
        }
    }
    if let Some(last) = current {
        out.push(last);
    }
    out
}

/// Whether two paths name the same directory.
///
/// **Not `==`.** A temporary directory on macOS is `/var/folders/…`, `/var` is a symlink to
/// `/private/var`, and git reports the resolved form — so a literal comparison against the
/// path we asked for fails on exactly the platform this ships on first. Canonicalising both
/// sides fixes it; the raw comparison is the fallback for a path that no longer exists,
/// where `canonicalize` cannot answer at all.
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// A few names and a count, for a refusal message.
///
/// Bounded because a refusal that lists four hundred files is one nobody reads, and the
/// count is what tells the user whether the three shown are the whole story.
fn summarise(paths: &[String]) -> String {
    const SHOWN: usize = 3;
    let head = paths.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > SHOWN {
        format!("{head} and {} more", paths.len() - SHOWN)
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Every test that needs a repository begins with this.
    ///
    /// Git is not a build dependency of this crate — it is a thing the *user's machine* may
    /// have — so a machine without it must skip rather than fail. Skipping silently is the
    /// lesser evil against a red suite that says nothing about this code.
    fn git_present() -> bool {
        available()
    }

    /// A repository under a directory whose name contains a space, on purpose: arguments go
    /// through `Command`'s argv and never a shell, and this is what proves it. With the
    /// path interpolated into a shell string instead, every one of these tests fails.
    fn repo_with_a_commit(inside: &Path) -> PathBuf {
        let repo = inside.join("a repo with spaces");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        fs::write(repo.join("README.md"), "hello").unwrap();
        run_git(&repo, &["add", "README.md"]);
        // A throwaway repository has no `user.name`, and `git commit` *fails* without one
        // rather than inventing a default. Passed with `-c` so nothing is written to the
        // machine's own git config.
        run_git(
            &repo,
            &[
                "-c",
                "user.name=velm test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "first",
            ],
        );
        repo
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn a_worktree_lands_on_its_own_branch() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = repo_with_a_commit(temp.path());
        let state = temp.path().join("state dir");

        let worktree = create(&repo, &state, "42@7").expect("worktree add");
        assert!(worktree.path.is_dir(), "the worktree directory was not created");
        assert!(worktree.path.join("README.md").is_file(), "the checkout is empty");
        assert_eq!(worktree.branch.as_deref(), Some("velm/42-7"));
        assert!(worktree.is_velm());

        // Asked of git rather than of our own return value: the branch we *say* we made and
        // the branch that is actually checked out are two different claims, and only the
        // second one is the feature.
        let head = run_git(&worktree.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(head.trim(), "velm/42-7");

        let listed = list(&repo).unwrap();
        assert!(
            listed.iter().any(|wt| wt.branch.as_deref() == Some("velm/42-7")),
            "the new worktree was not listed: {listed:?}"
        );
    }

    /// The rule the module exists to keep. An agent's uncommitted work is by definition the
    /// only copy, so removal has to refuse and has to say what it refused over.
    ///
    /// **Untracked, not modified**, because that is what an agent actually leaves behind — a
    /// check written against `git diff` alone would pass this file and delete the work.
    #[test]
    fn removing_a_worktree_with_uncommitted_work_is_refused_by_name() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = repo_with_a_commit(temp.path());
        let state = temp.path().join("state dir");
        let worktree = create(&repo, &state, "42@7").unwrap();

        fs::write(worktree.path.join("agent-notes.md"), "half a day's work").unwrap();
        assert_eq!(dirty(&worktree.path).unwrap(), vec!["agent-notes.md".to_owned()]);

        let refusal = remove(&repo, &worktree.path).expect_err("a dirty worktree was removed");
        let message = refusal.to_string();
        assert!(message.contains("agent-notes.md"), "the refusal did not name the file: {message}");
        assert!(
            worktree.path.join("agent-notes.md").is_file(),
            "the refusal still deleted the work"
        );
    }

    /// And the other half: a clean one goes, and **the branch stays**. The second assertion
    /// is the one that matters — a removal that also deleted `velm/42-7` would pass every
    /// check about the directory while throwing away every commit the agent made.
    #[test]
    fn a_clean_worktree_is_removed_and_its_branch_survives() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = repo_with_a_commit(temp.path());
        let state = temp.path().join("state dir");
        let worktree = create(&repo, &state, "42@7").unwrap();

        remove(&repo, &worktree.path).expect("a clean worktree was refused");
        assert!(!worktree.path.exists(), "the directory is still there");

        let branches = run_git(&repo, &["branch", "--list", "velm/42-7"]);
        assert!(branches.contains("velm/42-7"), "removal deleted the agent's branch");
    }

    /// After a removal the branch is still there, so a second `create` must attach to it
    /// rather than passing `-b` — which fails on an existing name — and must certainly not
    /// delete it to make room.
    #[test]
    fn a_second_create_reuses_the_branch_the_first_left_behind() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = repo_with_a_commit(temp.path());
        let state = temp.path().join("state dir");

        let first = create(&repo, &state, "42@7").unwrap();
        fs::write(first.path.join("work.txt"), "committed work").unwrap();
        run_git(&first.path, &["add", "work.txt"]);
        run_git(
            &first.path,
            &[
                "-c",
                "user.name=velm test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "agent work",
            ],
        );
        remove(&repo, &first.path).unwrap();

        let second = create(&repo, &state, "42@7").expect("the existing branch was not reused");
        assert_eq!(second.branch.as_deref(), Some("velm/42-7"));
        assert!(
            second.path.join("work.txt").is_file(),
            "the reattached worktree lost the commit the first one made"
        );
    }

    /// Re-opening a board must not fail because the agent's checkout is exactly where it
    /// was left, so a second create over a live worktree answers with that worktree.
    #[test]
    fn creating_over_a_live_worktree_returns_the_one_that_is_there() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = repo_with_a_commit(temp.path());
        let state = temp.path().join("state dir");

        let first = create(&repo, &state, "42@7").unwrap();
        let again = create(&repo, &state, "42@7").expect("a re-open was refused");
        assert_eq!(again.branch, first.branch);
        assert!(same_path(&again.path, &first.path));
    }

    /// The state a brand-new project is in, and the one failure with a remedy worth naming.
    /// `git worktree add`'s own message here is about an "invalid reference", which tells
    /// the user nothing about what to do.
    #[test]
    fn a_repository_with_no_commits_says_so_in_words() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("brand new");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        assert!(is_repo(&repo).unwrap());
        assert!(!has_commits(&repo).unwrap());

        let error = create(&repo, &temp.path().join("state"), "1@1").expect_err("added anyway");
        let message = error.to_string();
        assert!(message.contains("no commits"), "{message}");
        assert!(message.contains("commit first"), "the message did not say what to do: {message}");
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_named_rather_than_guessed_at() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_repo(temp.path()).unwrap());
        let error = create(temp.path(), &temp.path().join("state"), "1@1").expect_err("added");
        assert!(error.to_string().contains("not a git repository"), "{error}");
    }

    /// A detached HEAD is a state a user can leave a repository in, and it has commits, so
    /// it must work rather than be refused — `-b` branches from whatever HEAD names.
    #[test]
    fn a_detached_head_still_has_commits_to_branch_from() {
        if !git_present() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = repo_with_a_commit(temp.path());
        let head = run_git(&repo, &["rev-parse", "HEAD"]);
        run_git(&repo, &["checkout", "-q", "--detach", head.trim()]);

        assert!(has_commits(&repo).unwrap(), "a detached HEAD read as an unborn one");
        let worktree = create(&repo, &temp.path().join("state"), "9@9").expect("refused");
        assert_eq!(worktree.branch.as_deref(), Some("velm/9-9"));
    }

    /// The one security-shaped bug available in this module: an item id becomes a path
    /// component, so a `/` or a `..` in one would put an agent's checkout anywhere on disk.
    /// The whitelist is what stops it — this test fails on a blocklist that forgot a
    /// separator, and on any implementation that passes the id through unchanged.
    #[test]
    fn a_node_id_can_never_walk_out_of_the_state_directory() {
        let state = Path::new("/state");
        for hostile in ["../../etc/passwd", "/etc/passwd", "..", "a/b", "-rf", ""] {
            let path = path_for(state, hostile);
            assert_eq!(
                path.parent(),
                Some(Path::new("/state/wt")),
                "{hostile:?} escaped to {}",
                path.display()
            );
            let last = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            assert!(!last.is_empty(), "{hostile:?} produced an empty component");
            assert!(!last.starts_with('-'), "{hostile:?} produced a leading dash: {last}");
        }

        assert_eq!(slug("42@7"), "42-7");
        assert_eq!(slug(""), "agent");
        assert_eq!(slug("夕焼け"), "agent", "a non-ASCII id must not become an empty name");
        assert!(branch_for("42@7").starts_with(BRANCH_PREFIX));
    }

    /// The porcelain parser, without a repository. A `strip_prefix` typo here would show up
    /// only as an empty list at runtime, which reads exactly like "no worktrees yet".
    #[test]
    fn the_porcelain_list_parser_reads_branches_and_detached_heads() {
        let sample = "\
worktree /work/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /state/wt/42-7
HEAD 2222222222222222222222222222222222222222
branch refs/heads/velm/42-7

worktree /state/wt/loose
HEAD 3333333333333333333333333333333333333333
detached
";
        let parsed = parse_list(sample);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert!(!parsed[0].is_velm());
        assert_eq!(parsed[1].path, PathBuf::from("/state/wt/42-7"));
        assert_eq!(parsed[1].branch.as_deref(), Some("velm/42-7"));
        assert!(parsed[1].is_velm());
        assert_eq!(parsed[2].branch, None, "a detached HEAD reported a branch");
    }

    #[test]
    fn a_refusal_names_a_few_files_and_counts_the_rest() {
        assert_eq!(summarise(&["a".into(), "b".into()]), "a, b");
        let many: Vec<String> = (0..10).map(|n| format!("f{n}")).collect();
        assert_eq!(summarise(&many), "f0, f1, f2 and 7 more");
    }
}
