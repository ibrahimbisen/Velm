//! Notes are real files. The content lives in a `.md` on disk and nowhere else.
//!
//! `docs/07-agent-canvas.md` §8 is the contract. [`crate::NoteModel`] — the token in the
//! document — holds a **path**, a scope, the links it found and the mtime and length it last
//! saw. It does not hold a single character of the note, which is the entire feature: the
//! user can open the file in any editor, an agent can write it with an ordinary shell
//! redirect, and nothing has to be exported from Velm to be useful outside it.
//!
//! ```text
//!   <project>/.velm/notes/plan.md              shared — every agent on the board
//!   <project>/.velm/notes/42-7/scratch.md      private — the agent whose id slugs to 42-7
//!   <data-dir>/agents/<board-key>/notes/…      the fallback, for a board with no project
//! ```
//!
//! # The private scope is enforced here, not by the filesystem
//!
//! [`access_check`] is the boundary, and it takes the **requesting agent's id**. File
//! permissions were considered and are wrong for this: they would take the note away from
//! the user as well, and a note the user cannot open in their own editor defeats the reason
//! notes are files. The restriction is about which *agent* may be handed the contents, and
//! that question is only answerable at the API — so it is answered at the API.
//!
//! [`Requester::User`] is always allowed. The canvas is the user.
//!
//! # A note always has somewhere to live
//!
//! A board that is not inside a project directory still gets notes:
//! `<data-dir>/agents/<board-key>/notes/`, where `<board-key>` is the same hash of the
//! board's canonical path the transcript sidecar uses (§4), so a board's notes follow the
//! board and two boards never collide. [`NoteStore::locate`] is the one place that choice is
//! made; nothing else in the app should be deciding where a note goes.
//!
//! # Writes are atomic; debouncing is the caller's job
//!
//! [`write_atomically`] writes a temp file in the same directory and renames it over the
//! target, so an agent reading mid-write never sees half a file. It is a primitive and it is
//! called once per save — *when* to save is a question about a caret leaving a node, which
//! is `vellum-app`'s to answer.
//!
//! # ⚠ Who calls what today, and what [`NoteStore::save`] is waiting for
//!
//! Stated because the gap is invisible from inside this module and reads as a bug from
//! outside it. **Velm is read-only against a note's body.** These are the three live paths:
//!
//! - [`NoteStore::create`] — the canvas, when the user makes a note node. Writes once.
//! - [`NoteStore::reload`] — the canvas, on a low-frequency poll of the notes on screen.
//!   §8's *external edits win on a clean node*, and it is the only thing that reads a body.
//! - An agent's `note_write` over IPC, which goes to [`write_atomically`] **directly** and
//!   deliberately: an agent writing a note is a server-side operation with nobody holding an
//!   unsaved buffer, so last-write-wins with an atomic rename is the whole of what it needs,
//!   and `append` is offered exactly so two agents do not have to read-modify-write.
//!
//! So [`NoteStore::save`] — and with it [`Save::Conflict`] and the `<slug>.velm-conflict.md`
//! rule — **has no caller in the application.** It is not dead code and it is not aspirational:
//! it is §8's contracted behaviour for the one path that does not exist yet, a caret in a
//! note's *body*. There is none because `ItemKind::AgentNote` answers `text()` with its
//! **title**, so the on-canvas editor reaches the heading and nothing else. The day that
//! caret lands, saving is `save` and there is nothing to design.
//!
//! Two consequences worth knowing rather than rediscovering. Nothing can lose a note today,
//! because nothing writes one from a buffer. And a conflict file can still *exist* — an agent
//! may write one under that name, or one may survive from another tool — which is why
//! [`NoteStore::conflict_at`] asks the filesystem rather than assuming that no caller means
//! no conflict.

use std::collections::{BTreeSet, VecDeque};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{NoteModel, NoteScope};
use crate::{AgentError, Result};

/// The per-project directory Velm keeps its own files in.
pub const VELM_DIR: &str = ".velm";
/// The notes directory, under [`VELM_DIR`] in a project and under the board's own state
/// directory otherwise.
pub const NOTES_DIR: &str = "notes";
/// Where the fallback store lives under the app's data directory.
pub const AGENTS_DIR: &str = "agents";
/// What the losing side of a conflict is written as: `<slug>.velm-conflict.md`.
pub const CONFLICT_SUFFIX: &str = "velm-conflict.md";

/// How deep a link chain is followed by default.
///
/// Four, because a note that is four hops from the one an agent was pointed at is context
/// the user did not obviously ask for, and because the bound has to be small enough that a
/// densely linked set of notes cannot quietly become the whole board.
pub const MAX_LINK_DEPTH: usize = 4;

/// How many notes a chain may reach in total, whatever the depth allows.
///
/// Both bounds are needed: depth alone does not bound a note that links to thirty siblings,
/// and a total alone does not stop a long chain from being followed one note at a time.
pub const MAX_LINKED_NOTES: usize = 32;

/// Who is asking for a note.
///
/// The user is not an agent and is never refused — §8's *"the user must keep full access"*
/// is the whole reason the restriction is here rather than in the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requester<'a> {
    /// The person at the keyboard: the canvas, the inspector, an export.
    User,
    /// An agent, by its item id — the same string [`NoteScope::Private`] stores.
    Agent(&'a str),
}

/// Whether this requester may read or write a note in this scope.
///
/// A refusal names both agents, because *"access denied"* on a board of eight agents is not
/// something a user can act on. It is [`AgentError::Refused`] rather than a bool so a
/// refusal reaching an agent over IPC arrives as the sentence it will be shown.
pub fn access_check(scope: &NoteScope, by: Requester<'_>) -> Result<()> {
    match (scope, by) {
        (NoteScope::Shared, _) | (_, Requester::User) => Ok(()),
        (NoteScope::Private { agent }, Requester::Agent(who)) if who == agent => Ok(()),
        (NoteScope::Private { agent }, Requester::Agent(who)) => Err(AgentError::Refused(format!(
            "that note is private to {agent}; {who} may not read or write it"
        ))),
    }
}

/// Whether a note's file has changed since the model last looked at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The model has no stamp yet: it has never been read or written through this store.
    NeverSeen,
    /// Same mtime and same length. As close to "unchanged" as a stat can get.
    Unchanged,
    /// The mtime or the length moved.
    Changed,
    /// There is no file there — deleted underneath us, or never written.
    Missing,
}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Save {
    /// The buffer went to the note's own file.
    Written,
    /// The file already held exactly this text; nothing was written.
    Unchanged,
    /// The file changed underneath *and* the buffer differs from it. Both were kept: the
    /// external version stays at the note's own path and the canvas buffer was written to
    /// `kept`.
    Conflict { kept: PathBuf },
}

impl Save {
    /// The conflict file, when there was one — what the node says on itself.
    pub fn conflict(&self) -> Option<&Path> {
        match self {
            Self::Conflict { kept } => Some(kept.as_path()),
            _ => None,
        }
    }
}

/// How far a link chain may be followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLimits {
    pub depth: usize,
    pub total: usize,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self { depth: MAX_LINK_DEPTH, total: MAX_LINKED_NOTES }
    }
}

/// Where a board's notes live, and everything done to them.
///
/// Two paths, and the distinction matters: `root` is where note *files* go, and `base` is
/// what [`crate::NoteModel::path`] is stored relative to — the project root, so a project
/// that moves keeps working. `base` is `None` for the fallback store, where there is no
/// project to be relative to and paths are stored absolute; that is exactly what the model's
/// own doc comment promises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteStore {
    root: PathBuf,
    base: Option<PathBuf>,
}

impl NoteStore {
    /// `<project>/.velm/notes`, with paths stored relative to the project.
    pub fn for_project(project: &Path) -> Self {
        let base = normalise(project);
        Self { root: base.join(VELM_DIR).join(NOTES_DIR), base: Some(base) }
    }

    /// `<data-dir>/agents/<board-key>/notes`, for a board that is not inside a project.
    ///
    /// The key is slugged even though it arrives as a hash: it is a *parameter*, and one
    /// caller passing a board's title instead would otherwise put a slash in a directory
    /// name. Slugging a hex hash changes nothing.
    pub fn for_board_key(data_dir: &Path, board_key: &str) -> Self {
        let root = normalise(data_dir).join(AGENTS_DIR).join(slug(board_key)).join(NOTES_DIR);
        Self { root, base: None }
    }

    /// The one place the choice between the two is made.
    pub fn locate(project: Option<&Path>, data_dir: &Path, board_key: &str) -> Self {
        project.map_or_else(|| Self::for_board_key(data_dir, board_key), Self::for_project)
    }

    /// Where note files live.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// What stored paths are relative to, when they are relative at all.
    pub fn base(&self) -> Option<&Path> {
        self.base.as_deref()
    }

    /// The directory a note in this scope belongs in.
    ///
    /// **The layout is the scope**: a file directly in `root` is shared, and a file one
    /// directory down is private to the agent whose id slugs to that directory's name. This
    /// function and [`NoteStore::may_read_path`] are the two halves of that encoding and
    /// they have to agree — a note reached by *path* (a link, a chain) has no
    /// [`NoteScope`] to consult, so its privacy is read off where it is.
    pub fn dir_for(&self, scope: &NoteScope) -> PathBuf {
        match scope {
            NoteScope::Shared => self.root.clone(),
            NoteScope::Private { agent } => self.root.join(agent_dir(agent)),
        }
    }

    /// The file a note with this slug would have. The slug is re-slugged, which is
    /// idempotent, so a caller that passes a title by mistake gets a valid file rather than
    /// a broken path.
    pub fn path_for(&self, scope: &NoteScope, stem: &str) -> PathBuf {
        self.dir_for(scope).join(format!("{}.md", slug(stem)))
    }

    /// A slug that is free in this scope, appending `-2`, `-3` on collision — the rule
    /// `Library::free_path` already follows for board files, so two notes called *Plan* are
    /// two files rather than one overwritten one.
    pub fn free_stem(&self, scope: &NoteScope, title: &str) -> String {
        let stem = slug(title);
        let mut candidate = stem.clone();
        let mut n = 2u32;
        while self.path_for(scope, &candidate).exists() {
            candidate = format!("{stem}-{n}");
            n += 1;
        }
        candidate
    }

    /// The absolute path of a note, refusing one that escapes the store.
    ///
    /// A path in a board file is data, and a board can arrive from anywhere. `../../../.ssh/id_rsa`
    /// is not a note, and this is the one place that can say so — every read and write below
    /// goes through it.
    pub fn absolute(&self, note: &NoteModel) -> Result<PathBuf> {
        self.resolve_stored(&note.path)
    }

    /// Resolve a stored path — [`crate::NoteModel::path`]'s form — against this store.
    ///
    /// An empty path is refused rather than resolving to the notes directory itself, which
    /// is what joining nothing onto a base gives you: a note whose model has never been
    /// through [`NoteStore::create`] must not be able to address a *directory*.
    pub fn resolve_stored(&self, stored: &str) -> Result<PathBuf> {
        if stored.trim().is_empty() {
            return Err(AgentError::Refused("that note has no file yet".to_owned()));
        }
        let boundary = self.boundary();
        let candidate = Path::new(stored);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            boundary.join(candidate)
        };
        contain(&boundary, &joined)
            .ok_or_else(|| AgentError::Refused(format!("{stored} is outside this board's notes")))
    }

    /// A note's path in the form [`crate::NoteModel::path`] stores.
    ///
    /// Separators are normalised to `/` so a board written on Windows and opened on macOS
    /// still finds its notes. `Path` accepts `/` on both platforms, so nothing has to
    /// convert them back.
    pub fn stored_path(&self, absolute: &Path) -> String {
        let text = match &self.base {
            Some(base) => absolute
                .strip_prefix(base)
                .map_or_else(|_| absolute.to_string_lossy(), |rest| rest.to_string_lossy()),
            None => absolute.to_string_lossy(),
        };
        text.replace('\\', "/")
    }

    /// Create a note: pick a free slug from the title, write the file, and answer the model
    /// the document will hold.
    pub fn create(
        &self,
        title: &str,
        scope: NoteScope,
        text: &str,
        by: Requester<'_>,
    ) -> Result<NoteModel> {
        access_check(&scope, by)?;
        let stem = self.free_stem(&scope, title);
        let path = self.path_for(&scope, &stem);
        write_atomically(&path, text)?;

        let mut note = NoteModel {
            path: self.stored_path(&path),
            scope,
            links: self.links_from(&path, text),
            seen_mtime: None,
            seen_len: None,
        };
        self.stamp_into(&mut note, &path)?;
        Ok(note)
    }

    /// Read a note's file. Does not touch the model's stamp — a read that silently marked
    /// the note as seen would make [`NoteStore::freshness`] answer *unchanged* for an edit
    /// nobody has looked at.
    pub fn read(&self, note: &NoteModel, by: Requester<'_>) -> Result<String> {
        access_check(&note.scope, by)?;
        let path = self.absolute(note)?;
        std::fs::read_to_string(&path)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))
    }

    /// The conflict file beside a note, if one is there.
    ///
    /// `<slug>.velm-conflict.md`, asked of the filesystem rather than remembered: a conflict
    /// is a *file*, it outlives the session that produced it, and the node that reports it
    /// has to keep reporting it after a restart — which nothing in memory could do.
    ///
    /// # Only the canonical name is checked, and that bound is deliberate
    ///
    /// [`free_conflict_path`] appends `-2` only when the canonical file already exists, so a
    /// numbered conflict implies a canonical one and this cannot miss a live conflict. The one
    /// state it does not see is a user who deleted `plan.velm-conflict.md` by hand and kept
    /// `plan-2.velm-conflict.md`, which is somebody tidying up half way. The alternative is a
    /// prefix scan of the directory, and that is worse than the gap it closes: a note the user
    /// genuinely called *Plan 2* has a file called `plan-2.md`, and a scan cannot tell its
    /// conflict from this one's second.
    pub fn conflict_at(&self, stored: &str) -> Option<PathBuf> {
        let path = self.resolve_stored(stored).ok()?;
        let conflict = conflict_path(&path);
        conflict.exists().then_some(conflict)
    }

    /// The two filesystem facts a note node reports about itself: whether its file is there,
    /// and whether a conflict file sits beside it.
    ///
    /// Answered here rather than by the caller joining paths for itself, because "where does
    /// this stored path resolve to" is [`NoteStore::resolve_stored`]'s question and a second
    /// answer to it is how a note that exists comes to report that it does not — the store's
    /// base is only sometimes what a path is relative to.
    pub fn state_of(&self, stored: &str) -> (bool, bool) {
        let on_disk = self.resolve_stored(stored).is_ok_and(|path| path.exists());
        (on_disk, self.conflict_at(stored).is_some())
    }

    /// Whether the file changed since the model last saw it.
    ///
    /// **mtime *and* length**, because a filesystem with one-second mtime granularity cannot
    /// tell two edits in the same second apart — and a note is exactly the file an agent
    /// rewrites twice quickly. Neither alone is enough: a length-only check misses an edit
    /// that swaps two words, and an mtime-only check misses the second write of the second.
    pub fn freshness(&self, note: &NoteModel) -> Result<Freshness> {
        let path = self.absolute(note)?;
        Ok(compare(note, &path))
    }

    /// Re-read the file, take its stamp and re-derive its links. The *external edits win*
    /// half of §8: a note nobody is editing on the canvas simply follows its file.
    pub fn reload(&self, note: &mut NoteModel, by: Requester<'_>) -> Result<String> {
        let text = self.read(note, by)?;
        let path = self.absolute(note)?;
        note.links = self.links_from(&path, &text);
        self.stamp_into(note, &path)?;
        Ok(text)
    }

    /// Write a canvas buffer back, keeping both sides when the file moved underneath it.
    ///
    /// # Which side keeps the note's own path
    ///
    /// The external version does. The file is the source of truth — that is the feature —
    /// and an external writer may be another agent, a formatter or the user's editor;
    /// finding its write replaced by a buffer that was stale before it started is the one
    /// outcome nothing can recover from. The canvas buffer is written beside it as
    /// `<slug>.velm-conflict.md`, the node reports it, and **nothing is discarded**.
    ///
    /// The conflict file itself gets the `-2` treatment on collision: a second conflict must
    /// not overwrite the first, which would make the mechanism that exists to lose nothing
    /// the thing that lost it.
    pub fn save(&self, note: &mut NoteModel, text: &str, by: Requester<'_>) -> Result<Save> {
        access_check(&note.scope, by)?;
        let path = self.absolute(note)?;

        let on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(AgentError::file(path.display().to_string(), &error)),
        };

        // The file already says exactly this: nothing to write, but the stamp is now stale
        // and would report a change forever.
        if on_disk.as_deref() == Some(text) {
            note.links = self.links_from(&path, text);
            self.stamp_into(note, &path)?;
            return Ok(Save::Unchanged);
        }

        // Changed underneath *and* different from the buffer. `NeverSeen` counts: a model
        // with no stamp cannot claim the file it is about to overwrite is its own.
        let moved = matches!(compare(note, &path), Freshness::Changed | Freshness::NeverSeen);
        if let (true, Some(disk)) = (moved, on_disk) {
            let kept = free_conflict_path(&path);
            write_atomically(&kept, text)?;
            // The node now shows the external version, so its links and stamp come from
            // that — or the conflict would re-fire on every poll.
            note.links = self.links_from(&path, &disk);
            self.stamp_into(note, &path)?;
            return Ok(Save::Conflict { kept });
        }

        write_atomically(&path, text)?;
        // Re-derived on every save, not only on create and reload: this is the main write
        // path, and a cached chain that only updates on reload goes stale the moment
        // somebody edits a note on the canvas.
        note.links = self.links_from(&path, text);
        self.stamp_into(note, &path)?;
        Ok(Save::Written)
    }

    /// Every note reachable from this one by markdown link, breadth first, this one first.
    ///
    /// # The bounds, and why there are two of them
    ///
    /// [`LinkLimits`] defaults to depth [`MAX_LINK_DEPTH`] and [`MAX_LINKED_NOTES`] in total.
    /// An unbounded traversal is a hang on a cyclic note graph, and **a cycle here is not a
    /// mistake** — two notes that reference each other is the normal shape of a set of
    /// notes, not a corruption to be repaired. So cycles are handled rather than prevented:
    /// a note already visited is not visited again, and the walk ends.
    ///
    /// # Foreign private notes are skipped, not refused
    ///
    /// A shared note may link to another agent's private one. That link is followed for its
    /// owner and for the user, and silently skipped for anybody else — a refusal would let
    /// one link anywhere in the graph destroy the whole chain, which is a denial of service
    /// dressed as a permission check.
    pub fn linked_closure(
        &self,
        note: &NoteModel,
        by: Requester<'_>,
        limits: LinkLimits,
    ) -> Result<Vec<String>> {
        Ok(self.walk(note, by, limits)?.into_iter().map(|(path, _)| path).collect())
    }

    /// The chain of context an agent gets when it is asked to follow a note's links: every
    /// reachable note as `(stored path, contents)`, in the order
    /// [`NoteStore::linked_closure`] visits them.
    ///
    /// The contents come out of the same walk that found the paths rather than a second read
    /// afterwards — a note can be deleted between the two, and a chain that says a note is
    /// reachable and then cannot produce it is worse than one that never mentioned it.
    pub fn context_chain(
        &self,
        note: &NoteModel,
        by: Requester<'_>,
        limits: LinkLimits,
    ) -> Result<Vec<(String, String)>> {
        self.walk(note, by, limits)
    }

    /// Breadth-first over the link graph, reading each note once.
    fn walk(
        &self,
        note: &NoteModel,
        by: Requester<'_>,
        limits: LinkLimits,
    ) -> Result<Vec<(String, String)>> {
        access_check(&note.scope, by)?;
        let start = self.absolute(note)?;
        let boundary = self.boundary();

        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        let mut found: Vec<(String, String)> = Vec::new();
        queue.push_back((start.clone(), 0));

        while let Some((path, depth)) = queue.pop_front() {
            if found.len() >= limits.total {
                break;
            }
            // Marked visited before it is read, so a cycle ends whether or not the note it
            // points back at can be opened.
            if !seen.insert(path.clone()) {
                continue;
            }
            // The start note was checked against its own scope; everything reached by a link
            // is checked against where it lives. Compared against `start` rather than
            // against "nothing found yet" — an unreadable start would otherwise promote the
            // first note it links to into the start's exemption.
            if path != start && !self.may_read_path(&path, by) {
                continue;
            }
            // A link to a note that is not there yet is a dangling link, not a failure — the
            // agent that was going to write it may not have run — and a note that does not
            // exist is not part of the chain.
            let Ok(text) = std::fs::read_to_string(&path) else { continue };

            if depth < limits.depth {
                let from = path.parent().unwrap_or(self.root.as_path());
                for target in links_in(&text) {
                    if let Some(resolved) = contain(&boundary, &from.join(target)) {
                        queue.push_back((resolved, depth + 1));
                    }
                }
            }
            found.push((self.stored_path(&path), text));
        }

        Ok(found)
    }

    /// Whether a requester may read the note *at this path*, with the scope inferred from
    /// where the file is.
    ///
    /// See [`NoteStore::dir_for`]: the layout is the encoding, and these two must agree. A
    /// file deeper than one directory below the root is not a shape this store creates, so
    /// it is treated as private to nobody — the user may read it, an agent may not.
    ///
    /// # ⚠ There are two notions of "in bounds" in this file, and they are not the same one
    ///
    /// This function bounds to [`NoteStore::root`] — the *notes directory* — while
    /// [`NoteStore::resolve_stored`] bounds to [`NoteStore::boundary`], which is the **project
    /// root** when there is one. That is deliberate on both sides and is stated here because
    /// the app calls both and nothing else says so:
    ///
    /// - `resolve_stored` is the wider one on purpose. Its own doc gives the reason: a note
    ///   node pointed at a `docs/plan.md` the project already has is a reasonable thing to
    ///   want and is not an escape. It answers *"is this path a legal target"*.
    /// - This one is the narrower, and it answers a different question — *"may **this
    ///   requester** read it"* — from the file's **position**, because position is how scope
    ///   is encoded (`root/x.md` is shared, `root/<agent>/x.md` is private to that agent).
    ///
    /// So a project file outside the notes directory resolves and is then readable by the
    /// **user** and not by an **agent**, which is the safe direction and the intended one.
    /// The hazard is the reverse reading: **neither is a substitute for the other**, and a
    /// change that made one call the other — or that "unified" them onto `boundary()` — would
    /// silently make every file in the project readable by every agent on the board, because
    /// a path anywhere under the project root has no parent equal to `root` and would fall
    /// through this function's `Requester::Agent` arm. Two questions, two answers, both
    /// asked.
    pub fn may_read_path(&self, path: &Path, by: Requester<'_>) -> bool {
        let Some(dir) = path.parent() else { return false };
        if dir == self.root.as_path() {
            return true;
        }
        match by {
            Requester::User => true,
            Requester::Agent(who) => {
                let mine = agent_dir(who);
                dir.parent() == Some(self.root.as_path())
                    && dir.file_name().and_then(|name| name.to_str()) == Some(mine.as_str())
            }
        }
    }

    /// The links in `text`, resolved against the note's own directory and stored in
    /// [`crate::NoteModel::path`]'s form, so a private note's `../plan.md` is cached as the
    /// path everything else uses.
    fn links_from(&self, note_path: &Path, text: &str) -> Vec<String> {
        let from = note_path.parent().unwrap_or(self.root.as_path());
        let boundary = self.boundary();
        let mut out: Vec<String> = Vec::new();
        for target in links_in(text) {
            let Some(resolved) = contain(&boundary, &from.join(target)) else { continue };
            let stored = self.stored_path(&resolved);
            if !out.contains(&stored) {
                out.push(stored);
            }
        }
        out
    }

    /// What a path may not escape: the project when there is one, the store's own directory
    /// otherwise. The project rather than the notes directory, because a note node pointed
    /// at a `docs/plan.md` the project already has is a reasonable thing to want and is not
    /// an escape.
    fn boundary(&self) -> PathBuf {
        self.base.clone().unwrap_or_else(|| self.root.clone())
    }

    fn stamp_into(&self, note: &mut NoteModel, path: &Path) -> Result<()> {
        let (mtime, len) = stamp(path)?;
        note.seen_mtime = Some(mtime);
        note.seen_len = Some(len);
        Ok(())
    }
}

/// The directory name a private note's agent gets.
///
/// An item id is a Loro `TreeID` — a counter, an `@` and a peer id — so folding it through
/// [`slug`] turns `42@7` into `42-7` and cannot collide with another id: no `TreeID`
/// contains a `-` for the fold to run into.
pub fn agent_dir(agent: &str) -> String {
    slug(agent)
}

/// A file's modification time in whole seconds, and its length.
///
/// Reading a *file's* time is not reading the clock — the crate's rule is that logic which
/// depends on "now" takes it as a parameter, and nothing here does. A filesystem that cannot
/// answer a modification time reports 0, which leaves the length doing the work; that is
/// degraded, not broken, and it is why both are stored.
pub fn stamp(path: &Path) -> Result<(u64, u64)> {
    let meta = std::fs::metadata(path)
        .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs());
    Ok((mtime, meta.len()))
}

/// Write a file so that a reader sees the old contents or the new contents and never a
/// prefix of either.
///
/// Temp file in the **same directory**, then rename: a rename is atomic only within a
/// filesystem, and a temp directory can be on another one. The temp name carries the process
/// id and a counter, so two saves in flight in one process — or two Velms — cannot collide.
///
/// `sync_all` before the rename, because a rename is atomic with respect to *readers* and is
/// not a durability barrier: without it a crash can leave the rename applied and the bytes
/// not, which is a zero-length note. It costs one flush on a file measured in kilobytes.
///
/// On Windows the rename replaces an existing file, but fails if that file is held open by
/// another process. The error is reported rather than retried — a note that could not be
/// saved must say so, and a retry loop here would block a frame.
pub fn write_atomically(path: &Path, text: &str) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|error| AgentError::file(dir.display().to_string(), &error))?;

    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("note.md");
    let temp = dir.join(format!(
        ".{name}.velm-tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();

    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(AgentError::file(path.display().to_string(), &error));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(AgentError::file(path.display().to_string(), &error));
    }
    Ok(())
}

/// The markdown links in a note that point at a sibling note.
///
/// Ordinary inline links — `[text](other.md)` — with the two spellings that turn up in real
/// markdown: an angle-bracketed target `[t](<a note.md>)` and a title `[t](other.md "Plan")`.
/// A fragment is dropped, so `[t](plan.md#today)` is a link to `plan.md`.
///
/// Deliberately **not** followed: anything with a scheme (`https:`, `mailto:`), an absolute
/// path, an anchor-only `#section`, an image `![alt](x.md)`, and reference-style
/// `[t][ref]` — the last because resolving it needs a second pass over the definitions and a
/// note that links by reference is not a shape anything in Velm writes. Every one of those
/// is a link to something that is not a sibling note, and following it would put a web page
/// or a file from somewhere else into an agent's context under the name of a note.
pub fn links_in(markdown: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = markdown;
    let mut consumed = 0usize;

    while let Some(open) = rest.find('[') {
        // Every index below comes from `find` on an ASCII byte, so it is a char boundary —
        // and `get` is used regardless. `strip_site_affix` aborted the whole process twice
        // by slicing text like this (feedback 30), and this text is a file on disk.
        let before = markdown.get(..consumed + open).unwrap_or_default();
        let is_image = before.ends_with('!');
        let after = rest.get(open + 1..).unwrap_or_default();
        let Some(close) = after.find(']') else { break };
        let tail = after.get(close + 1..).unwrap_or_default();

        let mut advance = open + 1 + close + 1;
        if let Some(inside) = tail.strip_prefix('(')
            && let Some(end) = inside.find(')')
        {
            let target = inside.get(..end).unwrap_or_default();
            let link = if is_image { None } else { sibling_note(target) };
            match link {
                Some(link) if !out.contains(&link) => out.push(link),
                _ => {}
            }
            advance += 1 + end + 1;
        }

        consumed += advance;
        rest = markdown.get(consumed..).unwrap_or_default();
    }

    out
}

/// Reduce a link target to a sibling note's relative path, or refuse it.
fn sibling_note(target: &str) -> Option<String> {
    let target = target.trim();
    let target = match target.strip_prefix('<') {
        // The bracketed form exists precisely to allow a space in the name, so the title
        // split must not run on it — `<a note.md>` is one target, not two words.
        Some(inner) => inner.split('>').next()?,
        // A title after the target: `other.md "Plan"`.
        None => target.split_whitespace().next()?,
    };
    // A fragment: `other.md#today`.
    let target = target.split('#').next()?;
    if target.is_empty() {
        return None;
    }
    // A scheme — `https://`, `mailto:` — is a colon before any slash.
    if target.find(':').is_some_and(|at| target.get(..at).is_some_and(|s| !s.contains('/'))) {
        return None;
    }
    if target.starts_with('/') || target.starts_with('\\') {
        return None;
    }
    if !target.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    Some(target.to_owned())
}

/// A file name for a title: lower-case ASCII, safe on every filesystem Velm ships on.
///
/// # Why it folds a few letters instead of dropping every non-ASCII character
///
/// The user's own alphabet is Turkish, and without a fold *Bütçe planı* slugs to `b-t-e-plan`
/// — which is not a name, and two different titles collapse to it. The table covers Turkish
/// and the common Western European letters; everything else still becomes a separator, so a
/// title with no Latin letters at all yields `note`, `note-2`. That is ugly and it is
/// deliberate: **the file name is not the title**. The node carries the real one, the file
/// only has to be findable, typable and identical on macOS and Windows.
///
/// Never empty, never a reserved Windows device name, and capped at 64 characters so a
/// pasted paragraph does not become a path nothing can open.
pub fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if let Some(folded) = fold_latin(ch) {
            out.push_str(folded);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }

    // Capped *after* trimming, and trimmed again: a cut that lands on a separator would
    // leave a name ending in `-`, which Windows quietly strips and macOS does not — two
    // machines with two different file names for one note.
    let capped: String = out.trim_matches('-').chars().take(64).collect();
    let capped = capped.trim_end_matches('-');
    let mut stem = if capped.is_empty() { "note".to_owned() } else { capped.to_owned() };

    // `CON.md` is still the console on Windows, and Velm ships there. A suffix rather than a
    // rejection, so the note still carries something like the name the user typed.
    if is_reserved(&stem) {
        stem.push_str("-note");
    }
    stem
}

/// The Windows device names, which are reserved whatever extension follows them.
fn is_reserved(stem: &str) -> bool {
    if matches!(stem, "con" | "prn" | "aux" | "nul") {
        return true;
    }
    // `com1`…`com9` and `lpt1`…`lpt9`, and nothing longer: `common` is a perfectly good name.
    let Some(prefix) = stem.get(..3) else { return false };
    matches!(prefix, "com" | "lpt")
        && stem.len() == 4
        && stem.chars().last().is_some_and(|last| ('1'..='9').contains(&last))
}

/// Latin letters worth keeping as letters rather than turning into separators.
fn fold_latin(ch: char) -> Option<&'static str> {
    Some(match ch {
        'ç' | 'Ç' => "c",
        'ğ' | 'Ğ' => "g",
        'ı' | 'İ' => "i",
        'ö' | 'Ö' => "o",
        'ş' | 'Ş' => "s",
        'ü' | 'Ü' => "u",
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => "a",
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => "e",
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => "i",
        'ó' | 'ò' | 'ô' | 'õ' | 'Ó' | 'Ò' | 'Ô' | 'Õ' => "o",
        'ú' | 'ù' | 'û' | 'Ú' | 'Ù' | 'Û' => "u",
        'ñ' | 'Ñ' => "n",
        'ß' => "ss",
        _ => return None,
    })
}

/// Compare a model's stamp against the file.
fn compare(note: &NoteModel, path: &Path) -> Freshness {
    let Ok((mtime, len)) = stamp(path) else { return Freshness::Missing };
    match (note.seen_mtime, note.seen_len) {
        (Some(seen_mtime), Some(seen_len)) if seen_mtime == mtime && seen_len == len => {
            Freshness::Unchanged
        }
        (None, None) => Freshness::NeverSeen,
        _ => Freshness::Changed,
    }
}

/// `<stem>.velm-conflict.md` beside `path` — the name a conflicted save writes first.
///
/// Split out so the spelling exists once: [`free_conflict_path`] writes it and
/// [`NoteStore::conflict_at`] reads it, and a reader that spelled the suffix for itself would
/// be a node that stops reporting conflicts the day the constant changes.
fn conflict_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("note");
    dir.join(format!("{stem}.{CONFLICT_SUFFIX}"))
}

/// `<stem>.velm-conflict.md`, and `-2` if that is taken.
fn free_conflict_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("note");
    let mut candidate = conflict_path(path);
    let mut n = 2u32;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}-{n}.{CONFLICT_SUFFIX}"));
        n += 1;
    }
    candidate
}

/// Resolve `path` lexically and answer it only if it stays under `boundary`.
///
/// **Lexical, not canonical.** `canonicalize` needs the file to exist, and a note is
/// resolved before it is written. The cost is that a symlink inside the notes directory
/// pointing outside it is not caught — that is the user's own directory and they are allowed
/// to arrange it that way; what this closes is a *path in a board file* reaching out of the
/// project with `..`, which is the one that arrives from somewhere else.
fn contain(boundary: &Path, path: &Path) -> Option<PathBuf> {
    let resolved = normalise(path);
    resolved.starts_with(normalise(boundary)).then_some(resolved)
}

/// Remove `.` and resolve `..` without touching the filesystem.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // A `..` with nothing to pop stays, so it fails the containment check rather
                // than silently becoming the boundary itself.
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn store() -> (tempfile::TempDir, NoteStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::for_project(dir.path());
        (dir, store)
    }

    /// The primitive's whole job: a reader must see one whole version or another, never a
    /// prefix. A reader thread watches the file while it is rewritten fifty times.
    ///
    /// **A/B**: with `std::fs::write` in place of the temp-and-rename this fails within a few
    /// iterations — `write` truncates first, so a reader lands on a partial file almost
    /// immediately at this size.
    #[test]
    fn an_atomic_write_is_never_observed_half_written() {
        let (dir, _) = store();
        let path = dir.path().join("note.md");
        let a = "A".repeat(200_000);
        let b = "B".repeat(180_000);
        write_atomically(&path, &a).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (path, stop, a, b) = (path.clone(), stop.clone(), a.clone(), b.clone());
            std::thread::spawn(move || {
                let (mut reads, mut whole) = (0usize, 0usize);
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        reads += 1;
                        if text == a || text == b {
                            whole += 1;
                        }
                    }
                }
                (reads, whole)
            })
        };

        for i in 0..50 {
            write_atomically(&path, if i % 2 == 0 { &b } else { &a }).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let (reads, whole) = watcher.join().unwrap();

        assert!(reads > 0, "the reader never managed to look at the file");
        assert_eq!(reads, whole, "a reader saw a partially written note ({reads} reads)");

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("velm-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files were left behind: {leftovers:?}");
    }

    /// The private scope, at the API boundary where §8 puts it. The user is never refused —
    /// a note they cannot open would defeat the reason notes are files.
    #[test]
    fn a_private_note_refuses_another_agent_and_never_the_user() {
        let (_dir, store) = store();
        let mine = NoteScope::Private { agent: "42@7".into() };

        assert!(access_check(&mine, Requester::Agent("42@7")).is_ok());
        assert!(access_check(&mine, Requester::User).is_ok());
        assert!(access_check(&NoteScope::Shared, Requester::Agent("9@9")).is_ok());

        let refusal = access_check(&mine, Requester::Agent("9@9")).unwrap_err();
        let text = refusal.to_string();
        assert!(text.contains("42@7") && text.contains("9@9"), "{text}");

        let note = store.create("Scratch", mine, "mine alone", Requester::Agent("42@7")).unwrap();
        assert!(store.read(&note, Requester::Agent("42@7")).is_ok());
        assert!(store.read(&note, Requester::User).is_ok());
        assert!(store.read(&note, Requester::Agent("9@9")).is_err(), "a private note leaked");
        assert!(store.save(&mut note.clone(), "x", Requester::Agent("9@9")).is_err());

        // And it is one directory down, which is what makes the path-only check below work.
        let path = store.absolute(&note).unwrap();
        assert_eq!(path.parent().unwrap(), store.root().join("42-7"));
    }

    /// A shared note linking to somebody else's private one must not hand it over — the
    /// scope check reads the *location*, because a note reached by a link has no
    /// [`NoteScope`] to consult. Skipped rather than refused, so one link cannot kill a
    /// whole chain.
    #[test]
    fn a_chain_skips_another_agents_private_note_and_still_finishes() {
        let (_dir, store) = store();
        let owned_by = NoteScope::Private { agent: "42@7".into() };
        let secret =
            store.create("Secret", owned_by, "hidden", Requester::User).unwrap();
        let tail = store.create("Tail", NoteScope::Shared, "the end", Requester::User).unwrap();
        let hub = store
            .create(
                "Hub",
                NoteScope::Shared,
                "see [s](42-7/secret.md) and then [t](tail.md)",
                Requester::User,
            )
            .unwrap();

        let owner = store.linked_closure(&hub, Requester::Agent("42@7"), LinkLimits::default());
        assert!(owner.unwrap().contains(&secret.path), "the owner was locked out of its own note");

        let stranger =
            store.linked_closure(&hub, Requester::Agent("9@9"), LinkLimits::default()).unwrap();
        assert!(!stranger.contains(&secret.path), "a private note leaked through a link");
        assert!(stranger.contains(&tail.path), "one refused link ended the whole chain");

        let user = store.linked_closure(&hub, Requester::User, LinkLimits::default()).unwrap();
        assert!(user.contains(&secret.path), "the user was refused their own file");
    }

    /// Changed on both sides: both are kept, the external version keeps the note's own path,
    /// and the conflict is reported rather than being something the user has to notice.
    #[test]
    fn a_conflict_keeps_both_sides() {
        let (_dir, store) = store();
        let mut note =
            store.create("Plan", NoteScope::Shared, "the original", Requester::User).unwrap();
        let path = store.absolute(&note).unwrap();

        // Somebody else writes it — a different length, which is what makes this detectable
        // inside one second of mtime granularity.
        std::fs::write(&path, "written by an agent, at some length").unwrap();

        let saved = store.save(&mut note, "typed on the canvas", Requester::User).unwrap();
        let kept = saved.conflict().expect("a conflicting save reported no conflict").to_path_buf();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "written by an agent, at some length",
            "the external write was clobbered by a stale buffer"
        );
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "typed on the canvas");
        assert!(kept.file_name().unwrap().to_string_lossy().ends_with(CONFLICT_SUFFIX));

        // The stamp followed the file, so the same conflict does not re-fire on every poll.
        assert_eq!(store.freshness(&note).unwrap(), Freshness::Unchanged);

        // A second conflict must not overwrite the first — the mechanism that exists to lose
        // nothing would be the thing that lost it.
        std::fs::write(&path, "and again, differently").unwrap();
        let again = store.save(&mut note, "a second canvas edit", Requester::User).unwrap();
        let second = again.conflict().expect("the second conflict was not reported").to_path_buf();
        assert_ne!(second, kept, "the second conflict overwrote the first");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "typed on the canvas");
    }

    /// The two facts a note node reports about itself, asked of the filesystem.
    ///
    /// The second used to be a literal `false` in `vellum-app`'s inspector, defended by *"only
    /// `save` writes a conflict file and nothing calls `save`"*. That is a statement about
    /// this process, and the question is about the disk: an agent's own `note_write` can put a
    /// file under that name, and one can survive from a session that is long gone. So the
    /// second half is driven here by a file written **by something other than `save`**, which
    /// is precisely the case the old reasoning could not see.
    ///
    /// A/B: it fails against a `conflict_at` that remembers rather than looks, and against the
    /// constant it replaces.
    #[test]
    fn a_note_reports_whether_it_is_on_disk_and_whether_a_conflict_sits_beside_it() {
        let (_dir, store) = store();
        let note = store.create("Plan", NoteScope::Shared, "the original", Requester::User).unwrap();

        assert_eq!(store.state_of(&note.path), (true, false), "a fresh note reported a conflict");
        assert_eq!(store.conflict_at(&note.path), None);

        // Not through `save`: this is a file somebody else left there.
        let beside = store.absolute(&note).unwrap().with_extension(CONFLICT_SUFFIX);
        std::fs::write(&beside, "the other side").unwrap();

        assert_eq!(store.state_of(&note.path), (true, true), "the conflict file was not seen");
        assert_eq!(store.conflict_at(&note.path).as_deref(), Some(beside.as_path()));

        // And a note whose file has been deleted underneath the board is neither.
        std::fs::remove_file(store.absolute(&note).unwrap()).unwrap();
        std::fs::remove_file(&beside).unwrap();
        assert_eq!(store.state_of(&note.path), (false, false));

        // A path that escapes the store is refused rather than answered about, which is why
        // this goes through `resolve_stored` rather than joining onto the base.
        assert_eq!(store.state_of("../../../.ssh/id_rsa"), (false, false));
        assert_eq!(store.state_of(""), (false, false), "an empty path addressed the directory");
    }

    /// A conflict written by `save` is the same file `conflict_at` looks for. Two spellings of
    /// one name is a node that stops reporting conflicts and nothing that says why.
    #[test]
    fn the_conflict_a_save_writes_is_the_one_the_node_looks_for() {
        let (_dir, store) = store();
        let mut note =
            store.create("Plan", NoteScope::Shared, "the original", Requester::User).unwrap();
        let path = store.absolute(&note).unwrap();
        std::fs::write(&path, "written by an agent, at some length").unwrap();

        let saved = store.save(&mut note, "typed on the canvas", Requester::User).unwrap();
        let kept = saved.conflict().expect("no conflict was reported").to_path_buf();

        assert_eq!(store.conflict_at(&note.path).as_deref(), Some(kept.as_path()));
        assert_eq!(store.state_of(&note.path), (true, true));
    }

    /// An ordinary save is a save: no conflict when nobody else touched the file.
    #[test]
    fn an_undisturbed_save_writes_and_does_not_cry_conflict() {
        let (_dir, store) = store();
        let mut note = store.create("Plan", NoteScope::Shared, "first", Requester::User).unwrap();

        assert_eq!(store.save(&mut note, "second", Requester::User).unwrap(), Save::Written);
        assert_eq!(store.read(&note, Requester::User).unwrap(), "second");
        assert_eq!(store.freshness(&note).unwrap(), Freshness::Unchanged);

        // Saving the same text again is not a write and not a conflict.
        assert_eq!(store.save(&mut note, "second", Requester::User).unwrap(), Save::Unchanged);
    }

    /// The pairing that the model's own comment exists for. A one-second-granularity
    /// filesystem cannot tell two edits in the same second apart, so length is checked too.
    ///
    /// This assertion is the one that fails on an mtime-only comparison: the stamp's mtime is
    /// taken from the file itself, so only the length is wrong.
    #[test]
    fn an_edit_with_the_same_mtime_but_a_different_length_is_a_change() {
        let (_dir, store) = store();
        let note =
            store.create("Plan", NoteScope::Shared, "the original", Requester::User).unwrap();
        let path = store.absolute(&note).unwrap();
        let (mtime, len) = stamp(&path).unwrap();

        let unchanged = NoteModel { seen_mtime: Some(mtime), seen_len: Some(len), ..note.clone() };
        assert_eq!(store.freshness(&unchanged).unwrap(), Freshness::Unchanged);

        let same_second =
            NoteModel { seen_mtime: Some(mtime), seen_len: Some(len + 1), ..note.clone() };
        assert_eq!(
            store.freshness(&same_second).unwrap(),
            Freshness::Changed,
            "an edit within one second of the last one was missed"
        );

        let never = NoteModel { seen_mtime: None, seen_len: None, ..note.clone() };
        assert_eq!(store.freshness(&never).unwrap(), Freshness::NeverSeen);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.freshness(&note).unwrap(), Freshness::Missing);
    }

    /// Two notes that reference each other is the normal shape of a set of notes, not a
    /// corruption. The walk has to end, visit each note once, and start where it was asked to.
    #[test]
    fn a_cyclic_link_chain_terminates() {
        let (_dir, store) = store();
        let a = store.create("A", NoteScope::Shared, "see [b](b.md)", Requester::User).unwrap();
        let b = store.create("B", NoteScope::Shared, "see [c](c.md)", Requester::User).unwrap();
        let c = store.create("C", NoteScope::Shared, "back to [a](a.md)", Requester::User).unwrap();

        let chain = store.linked_closure(&a, Requester::User, LinkLimits::default()).unwrap();
        assert_eq!(chain, vec![a.path.clone(), b.path.clone(), c.path.clone()]);
        assert_eq!(chain.iter().filter(|path| **path == a.path).count(), 1, "a cycle repeated");
    }

    /// Both bounds do something, and each on its own is insufficient — depth does not bound
    /// a note that links to thirty siblings, and a total does not stop a long chain.
    #[test]
    fn both_traversal_bounds_are_enforced() {
        let (_dir, store) = store();
        // A chain a-1 → a-2 → … → a-6.
        for n in 1..=6 {
            let text = if n < 6 { format!("next [x](a-{}.md)", n + 1) } else { String::new() };
            store.create(&format!("a-{n}"), NoteScope::Shared, &text, Requester::User).unwrap();
        }
        let head = NoteModel {
            path: store.stored_path(&store.path_for(&NoteScope::Shared, "a-1")),
            ..NoteModel::default()
        };

        let deep = store
            .linked_closure(&head, Requester::User, LinkLimits { depth: 2, total: 100 })
            .unwrap();
        assert_eq!(deep.len(), 3, "depth 2 should reach the start and two hops: {deep:?}");

        let capped = store
            .linked_closure(&head, Requester::User, LinkLimits { depth: 10, total: 4 })
            .unwrap();
        assert_eq!(capped.len(), 4, "the total was not enforced: {capped:?}");

        let whole = store.linked_closure(&head, Requester::User, LinkLimits::default()).unwrap();
        assert_eq!(whole.len(), 5, "the default depth of {MAX_LINK_DEPTH} reaches five notes");
    }

    /// The file name is not the title. It only has to be findable, typable, non-empty and
    /// identical on macOS and Windows — and it must not panic on a title in any alphabet.
    #[test]
    fn a_non_ascii_title_slugs_safely() {
        assert_eq!(slug("Bütçe planı"), "butce-plani", "the Turkish fold did not run");
        assert_eq!(slug("Şeyler & Şeyler"), "seyler-seyler");
        assert_eq!(slug("Café Münster"), "cafe-munster");
        assert_eq!(slug("メモ"), "note", "a title with no Latin letters must still name a file");
        assert_eq!(slug("🙂🙂"), "note");
        assert_eq!(slug(""), "note");
        assert_eq!(slug("   "), "note");
        assert_eq!(slug("---"), "note");
        assert_eq!(slug("Plan/2026: notes"), "plan-2026-notes");

        // Reserved on Windows whatever the extension is, and Velm ships there.
        assert_ne!(slug("CON"), "con");
        assert_ne!(slug("lpt1"), "lpt1");
        assert_eq!(slug("common"), "common", "a name that merely starts like a device was mangled");

        let long = "a".repeat(500);
        for title in ["日本語", "🙂", "", "ß", long.as_str(), "\u{0}"] {
            let stem = slug(title);
            assert!(!stem.is_empty(), "{title:?} slugged to nothing");
            assert!(stem.chars().count() <= 69, "{stem}");
            assert!(
                stem.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{stem} is not portable"
            );
        }

        // Idempotent, which is what makes `path_for` safe to call on a slug or a title.
        for title in ["Bütçe planı", "CON", "メモ", "Plan/2026: notes"] {
            assert_eq!(slug(&slug(title)), slug(title), "{title}");
        }
    }

    /// Two notes called *Plan* are two files, not one overwritten one — `Library::free_path`'s
    /// rule, applied where it matters more.
    #[test]
    fn a_colliding_title_takes_the_next_free_name() {
        let (_dir, store) = store();
        let first = store.create("Plan", NoteScope::Shared, "one", Requester::User).unwrap();
        let second = store.create("Plan", NoteScope::Shared, "two", Requester::User).unwrap();
        assert_ne!(first.path, second.path);
        assert!(second.path.ends_with("plan-2.md"), "{}", second.path);
        assert_eq!(store.read(&first, Requester::User).unwrap(), "one", "the first was clobbered");
    }

    /// A path in a board file is data, and a board can arrive from anywhere.
    #[test]
    fn a_stored_path_that_escapes_the_project_is_refused() {
        let (_dir, store) = store();
        for escape in ["../../.ssh/id_rsa", ".velm/notes/../../../etc/hosts", "/etc/hosts"] {
            let note = NoteModel { path: escape.into(), ..NoteModel::default() };
            assert!(store.absolute(&note).is_err(), "{escape} was accepted");
        }

        // A note pointed at a file the project already has is not an escape.
        let doc = NoteModel { path: "docs/plan.md".into(), ..NoteModel::default() };
        assert!(store.absolute(&doc).is_ok());

        // And an empty path is refused rather than resolving to the notes *directory*,
        // which is what joining nothing onto the base would otherwise give.
        assert!(store.absolute(&NoteModel::default()).is_err(), "an empty path named a directory");
    }

    /// A board with no project directory still gets notes, under the same board key the
    /// transcript sidecar uses. A note must always have somewhere to live.
    #[test]
    fn a_board_with_no_project_still_has_somewhere_to_put_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Vellum");
        let store = NoteStore::locate(None, &data, "abc123");
        assert_eq!(store.root(), data.join("agents").join("abc123").join("notes"));
        assert_eq!(store.base(), None, "a store with no project must record absolute paths");

        let note = store.create("Plan", NoteScope::Shared, "somewhere", Requester::User).unwrap();
        assert!(Path::new(&note.path).is_absolute(), "{}", note.path);
        assert_eq!(store.read(&note, Requester::User).unwrap(), "somewhere");
        assert_eq!(store.freshness(&note).unwrap(), Freshness::Unchanged);

        // And with a project, the same call takes the project's own directory.
        let project = NoteStore::locate(Some(dir.path()), &data, "abc123");
        assert_eq!(project.root(), normalise(dir.path()).join(".velm").join("notes"));
    }

    /// Only sibling notes. A web page, a mail address or a file from elsewhere would be put
    /// into an agent's context under the name of a note.
    #[test]
    fn only_sibling_notes_are_treated_as_links() {
        let markdown = "\
            see [plan](plan.md), [ctx](<a note.md>), [titled](other.md \"Other\"),\n\
            [anchored](plan.md#today), [up](../shared.md)\n\
            not [web](https://example.com/x.md), [mail](mailto:a@b.md), [abs](/etc/x.md),\n\
            not [pdf](spec.pdf), not ![shot](picture.md), not [ref][elsewhere]\n";
        assert_eq!(
            links_in(markdown),
            vec!["plan.md", "a note.md", "other.md", "../shared.md"],
            "a link that is not a sibling note was followed, or one that is was dropped"
        );

        // `plan.md#today` is the same note as `plan.md`, so it appears once.
        assert_eq!(links_in("[a](x.md) [b](x.md)"), vec!["x.md"]);
        assert_eq!(links_in("no links here at all"), Vec::<String>::new());
    }

    /// A private note may link up into the shared ones — that is a legitimate `..`, which is
    /// why containment is checked rather than `..` being banned outright.
    #[test]
    fn a_private_note_may_link_up_to_a_shared_one() {
        let (_dir, store) = store();
        let shared = store.create("Shared", NoteScope::Shared, "context", Requester::User).unwrap();
        let private = store
            .create(
                "Mine",
                NoteScope::Private { agent: "42@7".into() },
                "see [s](../shared.md)",
                Requester::Agent("42@7"),
            )
            .unwrap();

        assert_eq!(private.links, vec![shared.path.clone()], "the `..` link was not resolved");
        let chain = store
            .context_chain(&private, Requester::Agent("42@7"), LinkLimits::default())
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1], (shared.path.clone(), "context".to_owned()));
    }

    /// The links cache is re-derived on the main write path, not only on create and reload —
    /// otherwise the chain an agent follows is the one the note had when it was made.
    #[test]
    fn saving_re_derives_the_links_cache() {
        let (_dir, store) = store();
        let other = store.create("Other", NoteScope::Shared, "", Requester::User).unwrap();
        let mut note =
            store.create("Plan", NoteScope::Shared, "no links", Requester::User).unwrap();
        assert!(note.links.is_empty());

        store.save(&mut note, "now see [o](other.md)", Requester::User).unwrap();
        assert_eq!(note.links, vec![other.path.clone()], "a save did not re-derive the links");

        store.save(&mut note, "and now none", Requester::User).unwrap();
        assert!(note.links.is_empty(), "a stale link survived a save that removed it");
    }

    /// A link to a note that has not been written yet is a dangling link, not a failure: the
    /// agent that was going to write it may not have run.
    #[test]
    fn a_dangling_link_does_not_break_a_chain() {
        let (_dir, store) = store();
        let note = store
            .create("Hub", NoteScope::Shared, "[gone](missing.md) [here](hub.md)", Requester::User)
            .unwrap();
        let chain = store.linked_closure(&note, Requester::User, LinkLimits::default()).unwrap();
        assert_eq!(chain, vec![note.path.clone()], "a dangling link was followed or fatal");
    }
}
