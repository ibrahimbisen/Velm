//! The board library: every board on disk, listed without opening one.
//!
//! `docs/01-architecture.md` §5 puts a lightweight index inside each board's SQLite
//! file — title, item count, thumbnail hash, mtime — precisely so that a hundred
//! boards can be listed without replaying a hundred CRDT documents.
//! [`vellum_store::list_boards`] reads exactly that, and this module is the layer
//! above it: the user's filing, the four verbs on a board, and the five on a space.
//!
//! # What lives where
//!
//! A board's *content* is its own file. A board's *filing* — which space it is in,
//! whether it is starred — is not content: it is about the collection, it would be
//! lost the moment a board file were copied out, and putting it inside the board
//! would make every star a CRDT transaction. So it lives in one small JSON sidecar
//! beside the boards, along with the two or three window preferences that are also
//! about the collection rather than any board.
//!
//! # Renaming renames the board, not the file
//!
//! `docs/04-ui-reference.md` §5 shows Miro's rename, which changes the name on the
//! row. It does not move a file, and neither does this: spaces refer to boards by
//! path, so renaming the file would silently empty every space the board was in, and
//! a board opened from a shell alias would stop resolving. [`Library::rename`]
//! rewrites the title inside the document — which is also what the library row reads,
//! so the change shows up with no extra bookkeeping.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use crate::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use vellum_doc::Board;
use vellum_store::{BOARD_EXTENSION, BoardDb, BoardIndex};
use vellum_ui::{Accent, BoardCard, Space, ThemePreference};

/// The user's filing, and the handful of preferences that belong to the collection
/// rather than to any one board.
///
/// Every field defaults, so a missing or half-written sidecar degrades to "no spaces,
/// nothing starred, follow the system" instead of losing the library.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Filing {
    /// ⚠ **The user calls these Folders; this key stays `spaces` forever.**
    ///
    /// The interface was relabelled on *"rename spaces to Folders"* — every visible
    /// string says Folder now. The serialised name deliberately did **not** move, and
    /// neither did any Rust identifier: this is the key every existing `library.json`
    /// on the user's machine already uses, and `serde` would silently read a renamed
    /// field as *absent*, degrade to "no folders" per the doc comment above, and then
    /// `persist` would write that back — **erasing which folder every board is filed
    /// under**, with the boards themselves intact so nothing would look broken until the
    /// user went looking. See RULE ZERO in `CLAUDE.md`.
    ///
    /// A rename here is possible but is a *migration*: read both spellings, write the
    /// new one, keep the fallback for good. It has no user-visible payoff, which is why
    /// it was not done.
    spaces: Vec<SpaceRecord>,
    starred: Vec<PathBuf>,
    /// The boards in **Recently deleted**, and when each went in.
    ///
    /// *"when a person deletes their board make it so that it puts it in the recently
    /// deleted folder"* — and this is how it is done without ever touching the file, which
    /// is the whole point under RULE ZERO. Deleting records a path and a time here; the
    /// `.vellum` and its `-wal`/`-shm` stay exactly where they are, byte for byte.
    /// Restoring drops the record. **Nothing is moved**, because a move is a filesystem
    /// operation that can fail halfway, cross a volume or collide with a name already
    /// there, and a trash whose own mechanism can lose a board is not a trash.
    ///
    /// Nothing here expires either. Every other application's trash empties itself after
    /// thirty or ninety days; this one does not, and will not — the boards are a migration
    /// of ~58 real Miro boards that cannot be re-imported, and a timer that deletes one is
    /// exactly the thing RULE ZERO forbids. It empties when the user empties it.
    #[serde(default)]
    trashed: Vec<TrashedBoard>,
    /// `"light"`, `"dark"` or `"system"`.
    theme: Option<String>,
    /// **Gone from the interface, kept in the struct on purpose.** This was *View ▸ Grid*,
    /// a per-view show/hide with `⇧⌘G` on it, removed at the user's request — a board's
    /// pattern is the board's, and *No grid* is a row in View ▸ Grid. The field stays so a
    /// sidecar written by an older build still parses every key it holds; nothing reads it.
    /// See `app::canvas_pattern` for the whole reasoning.
    /// No `#[expect(dead_code)]`: serde's derive both reads and writes it, so it is not
    /// dead — it is simply not read by the application any more.
    #[serde(default)]
    grid: bool,
    minimap: bool,
    /// The in-app translucency switch `docs/05` §3a asks for *in addition* to the OS
    /// setting. `None` is "not chosen", which resolves to on.
    translucency: Option<bool>,
    /// Whether link cards may fetch their own titles and preview images.
    ///
    /// `None` is "not chosen", which resolves to **on**.
    ///
    /// It defaulted to *off* first, reasoning that fetching contacts a third party for every
    /// card and that opting in was the user's to make. That reasoning was sound and the result
    /// was wrong: a pasted link produced a grey box showing only its own URL, with nothing on
    /// screen to say why, and the user reported the feature as broken — *"i thought you built
    /// the embeds properly but i dont see previews i just see a box"*. A privacy default that
    /// reads as a bug is not a privacy win; it just gets switched on by someone who now trusts
    /// the feature less. The switch is still there, one row down in Preferences.
    link_previews: Option<bool>,
    /// Miro's **Align objects**: whether a move, a resize or a placement is pulled onto the
    /// edges, centres and even spacings of what is already on the board.
    ///
    /// `None` is "not chosen", which resolves to **on** — as it is in Miro, and as the user
    /// asked for it: *"i love how miro has implemented it … in the settings give an option
    /// to enable and disable it"*. A feature nobody can find is off for everyone, and this
    /// one announces itself the first time an edge lights up.
    align_objects: Option<bool>,
    /// ⚠ **The Agent Canvas's six settings — archived, still parsed, deliberately unread.**
    ///
    /// The layer moved to `archive/` at the user's word: *"i just want it to be archived for
    /// now … i just dont want it to be part of the app"*. Nothing in the application reads
    /// any of these now, and no control anywhere writes one.
    ///
    /// **They stay in this struct, and that is the sharpest edge in the whole archiving.**
    /// This user's `library.json` already holds all six keys. Deleting the fields would not
    /// fail the parse — `serde` reads a field with no home as *absent*, which is harmless on
    /// its own. It is [`Library::persist`] that does the damage, because it writes the whole
    /// struct back: the next star, rename or delete rewrites the sidecar **without** these
    /// keys and the user's answers are gone, with nothing to notice it by, since the boards
    /// themselves would be untouched. That is the reasoning `spaces` records above and what
    /// was done for `grid` when feedback 31 removed *View ▸ Grid*. See RULE ZERO in
    /// `CLAUDE.md`: archiving the *feature* must not archive what is already on disk.
    ///
    /// So they are read, carried, and written back unchanged. A later Agent Canvas picks the
    /// user's answers up where they were left rather than asking all six again.
    ///
    /// `speech` is a raw [`serde_json::Value`] because its type lived in the archived crate.
    /// A `Value` carries **every** key and value on disk back out again, including ones a
    /// future version adds, where any stand-in struct would silently discard the half it did
    /// not model on the very next `persist`. (`serde_json`'s default `Value::Object` is a
    /// `BTreeMap`, so the keys inside `speech` come back alphabetised rather than in their
    /// original order. Nothing is lost, and no reader of this file cares about order.)
    ///
    /// No `#[expect(dead_code)]`, for `grid`'s reason: serde's derive both reads and writes
    /// every one of them, so they are not dead — only unread by the application.
    agent_display: Option<String>,
    agent_provider: Option<String>,
    agent_chat_theme: Option<String>,
    browser_nodes: Option<bool>,
    worktrees: Option<bool>,
    speech: Option<serde_json::Value>,
    /// Miro's **Snap to grid**: whether a move, a resize or a placement lands on the
    /// board's own grid.
    ///
    /// `None` resolves to **off**, unlike `align_objects` above, and the asymmetry is the
    /// point. This is the strict one — Miro's own forums are full of complaints about it,
    /// which is why feedback 24 built the loose one first — so it is a thing you turn on
    /// when you want a board laid out against a rule, not the way a board behaves by
    /// default.
    snap_to_grid: Option<bool>,
    /// The grid every board wears: `"plain"`, `"dots"`, `"crosses"` or `"lines"`.
    ///
    /// **App-wide, at the user's instruction** — *"grid opacity and grid color and grid
    /// should apply to all of the boards not just to that board"*. A board still carries a
    /// `Background::pattern` of its own and that is what `None` here falls back to, so a
    /// board that chose one before this existed keeps drawing it until a global choice is
    /// made. From the first choice on, the grid is one setting for every board.
    ///
    /// A **string**, exactly as `theme` and `accent` are, and for the reason spelled out on
    /// `accent`: an enum serialised by variant order turns "Lines" into "Crosses" the day
    /// someone adds a pattern in the middle. An unknown tag degrades to the fallback.
    grid_pattern: Option<String>,
    /// What the grid is drawn in, packed `0xRRGGBBAA` — colour **and** transparency.
    ///
    /// One number rather than two keys because a colour and its alpha describe the same
    /// pixel, and two keys is two things that can disagree about it. The menu offers them
    /// as two controls (a named list, a slider) over this one value.
    ///
    /// `None` follows the theme's own grid ink, which is what every board has today and
    /// what keeps a future palette change reaching boards nobody has restyled.
    grid_color: Option<i64>,
    /// How much tint the floating chrome lays over the blurred board, `0..=255`.
    /// `None` is "not chosen", which follows the palette — so a default that moves
    /// later moves for everyone who never touched the slider.
    glass_opacity: Option<u8>,
    /// Which colour the primary accent wears: `"teal"`, `"red"` or `"blue"`.
    ///
    /// A string rather than the enum, exactly as `theme` above is, and for the same reason:
    /// a sidecar written by today's build is read by tomorrow's, and an enum serialised by
    /// its variant *order* turns "Blue" into "Red" the day someone adds a fourth colour in
    /// the middle. An unknown string falls back to the default rather than failing the whole
    /// file to parse, which is what `#[serde(default)]` on this struct is for.
    accent: Option<String>,
    /// The Velm server the last sign-in used, canonical — `https://boards.example.com/`.
    ///
    /// Not a secret, and the same shape as `theme` and `accent` above: a string, `Option`,
    /// degrading to `None`. It is stored so the Account page opens with the address already
    /// filled in, because it has to be typed again on every launch otherwise.
    ///
    /// ⚠ The reverse-compatibility note `spaces` and `grid` carry applies here too, in the
    /// direction that matters: an **older** build reading a sidecar with this key parses it
    /// as absent and its next `persist` writes the struct back without it. The person retypes
    /// an address. No board is touched, because nothing on this path opens a board file.
    sync_server: Option<String>,
    /// The username the last sign-in used. Not a secret either, and stored for the same
    /// reason and with the same caveat as `sync_server`.
    ///
    /// **The password is deliberately absent and there is no key for it.** `Filing` derives
    /// `Debug` and [`Library::persist`] writes pretty plaintext JSON into a directory that is
    /// in every Time Machine backup, which is exactly the posture `crate::options`' `OnceLock`
    /// exists to hold. See `crate::signin` for what that costs.
    sync_username: Option<String>,
    /// The board that was open when the app last closed, so the next launch can offer
    /// it rather than making the user find it again.
    last_board: Option<PathBuf>,
    /// `docs/04-ui-reference.md` §4's *Start view*: the camera a board opens at, as
    /// `[x, y, zoom]`. Filed here rather than in the document because it is a
    /// preference about how this machine looks at the board — putting it in the CRDT
    /// would make every camera move a candidate transaction.
    start_views: Vec<StartView>,
    /// Miro `.rtb` backups attached through **Import from Miro**, by path.
    ///
    /// **The path, never a copy.** The reference archive is 114MB and the user has ~58 of
    /// them; copying each into the app's own folder would be ~6.6GB duplicated on a machine
    /// that has run out of disk twice. Remembering where it is costs a line of JSON and
    /// gives the same result — an attachment that survives a restart.
    ///
    /// The trade is that moving or deleting the original breaks the link. That is the right
    /// way round: `Library::rescan` already prunes boards that have gone, and a missing
    /// archive costs pictures on a re-import rather than losing anything on disk.
    attached_archives: Vec<PathBuf>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct StartView {
    board: PathBuf,
    camera: [f64; 3],
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SpaceRecord {
    name: String,
    boards: Vec<PathBuf>,
    pinned: bool,
}

/// Every board on disk, plus the user's filing of them.
#[derive(Debug)]
pub struct Library {
    root: PathBuf,
    sidecar: PathBuf,
    filing: Filing,
    cards: Vec<BoardCard>,
}

impl Library {
    /// Opens the library rooted at `root`, creating the directory if it is not there.
    ///
    /// Never fails on a damaged sidecar: the filing is a convenience and the boards
    /// are the data. A sidecar that will not parse is logged and replaced by the
    /// defaults, which is recoverable; refusing to start is not.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        if let Err(error) = std::fs::create_dir_all(&root) {
            log::warn!("creating {}: {error}", root.display());
        }
        let sidecar = root.join("library.json");
        let filing = match std::fs::read(&sidecar) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                log::warn!("{} could not be read: {error}", sidecar.display());
                Filing::default()
            }),
            Err(_) => Filing::default(),
        };

        let mut library = Self { root, sidecar, filing, cards: Vec::new() };
        library.rescan();
        library
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-reads every board's index row. Cheap — one small SQLite query per file —
    /// and the only thing that makes a board created outside the app appear.
    pub fn rescan(&mut self) {
        let indexes = match vellum_store::list_boards(&self.root) {
            Ok(indexes) => indexes,
            Err(error) => {
                log::warn!("listing {}: {error}", self.root.display());
                Vec::new()
            }
        };
        let starred: BTreeSet<&PathBuf> = self.filing.starred.iter().collect();
        let trashed: BTreeMap<&PathBuf, SystemTime> =
            self.filing.trashed.iter().map(|t| (&t.path, t.when())).collect();
        self.cards = indexes
            .into_iter()
            .map(|index| card_of(index, &starred, &trashed))
            .collect();

        // Filing that points at a board which is no longer there is dropped rather
        // than kept: a space listing a deleted board would show a count that never
        // matches its contents.
        let live: BTreeSet<PathBuf> = self.cards.iter().map(|c| c.path.clone()).collect();
        self.filing.starred.retain(|p| live.contains(p));
        for space in &mut self.filing.spaces {
            space.boards.retain(|p| live.contains(p));
        }
    }

    pub fn cards(&self) -> &[BoardCard] {
        &self.cards
    }

    pub fn spaces(&self) -> Vec<Space> {
        self.filing
            .spaces
            .iter()
            .map(|s| Space {
                name: s.name.clone(),
                boards: s.boards.clone(),
                pinned: s.pinned,
            })
            .collect()
    }

    pub fn is_starred(&self, path: &Path) -> bool {
        self.filing.starred.iter().any(|p| p == path)
    }

    // ----- preferences that belong to the collection ------------------------

    pub fn theme_preference(&self) -> ThemePreference {
        match self.filing.theme.as_deref() {
            Some("light") => ThemePreference::Light,
            Some("dark") => ThemePreference::Dark,
            _ => ThemePreference::System,
        }
    }

    pub fn set_theme_preference(&mut self, preference: ThemePreference) {
        self.filing.theme = Some(
            match preference {
                ThemePreference::Light => "light",
                ThemePreference::Dark => "dark",
                ThemePreference::System => "system",
            }
            .to_owned(),
        );
        self.persist();
    }

    pub fn minimap(&self) -> bool {
        self.filing.minimap
    }

    pub fn set_view_toggles(&mut self, minimap: bool) {
        self.filing.minimap = minimap;
        self.persist();
    }

    pub fn translucency(&self) -> bool {
        self.filing.translucency.unwrap_or(true)
    }

    pub fn set_translucency(&mut self, on: bool) {
        self.filing.translucency = Some(on);
        self.persist();
    }

    /// Whether link cards may fetch. **On** unless the user has turned it off — see the field.
    pub fn link_previews(&self) -> bool {
        self.filing.link_previews.unwrap_or(true)
    }

    pub fn set_link_previews(&mut self, on: bool) {
        self.filing.link_previews = Some(on);
        self.persist();
    }

    /// Whether relative snapping is on. **On** unless the user has turned it off.
    pub fn align_objects(&self) -> bool {
        self.filing.align_objects.unwrap_or(true)
    }

    pub fn set_align_objects(&mut self, on: bool) {
        self.filing.align_objects = Some(on);
        self.persist();
    }

    /// Whether a move lands on the board's grid. **Off** unless the user has turned it on.
    pub fn snap_to_grid(&self) -> bool {
        self.filing.snap_to_grid.unwrap_or(false)
    }

    pub fn set_snap_to_grid(&mut self, on: bool) {
        self.filing.snap_to_grid = Some(on);
        self.persist();
    }

    /// The grid every board wears, or `None` for "not chosen — use each board's own".
    ///
    /// An unrecognised tag answers `None` rather than failing: a sidecar written by a newer
    /// build must leave the app drawing a normal grid, not no grid and not an error.
    pub fn grid_pattern(&self) -> Option<vellum_doc::Pattern> {
        self.filing.grid_pattern.as_deref().and_then(vellum_doc::Pattern::from_tag)
    }

    pub fn set_grid_pattern(&mut self, pattern: vellum_doc::Pattern) {
        self.filing.grid_pattern = Some(pattern.tag().to_owned());
        self.persist();
    }

    /// What the grid is drawn in — colour and alpha together — or `None` for the theme's.
    pub fn grid_color(&self) -> Option<vellum_doc::Color> {
        // `from_packed` refuses an out-of-range value, so a corrupt sidecar reads as
        // "no colour chosen" — the theme's grid — rather than as an arbitrary one.
        self.filing.grid_color.and_then(vellum_doc::Color::from_packed)
    }

    pub fn set_grid_color(&mut self, color: Option<vellum_doc::Color>) {
        self.filing.grid_color = color.map(vellum_doc::Color::to_packed);
        self.persist();
    }

    /// The `.rtb` backups attached through Import from Miro, **that still exist**.
    ///
    /// Filtered on the way out rather than pruned on a schedule, so a backup on a volume
    /// that happens to be unmounted today comes back when it is plugged in again — the same
    /// reason `Library` keeps a starred board it cannot currently see.
    pub fn attached_archives(&self) -> Vec<PathBuf> {
        self.filing.attached_archives.iter().filter(|p| p.exists()).cloned().collect()
    }

    /// Remembers an attached archive. Idempotent — attaching the same file twice is what a
    /// user does when they are not sure it worked the first time, and it must not stack up
    /// duplicate entries that then load the same 114MB archive twice.
    pub fn attach_archive(&mut self, path: &Path) {
        if self.filing.attached_archives.iter().any(|p| p == path) {
            return;
        }
        self.filing.attached_archives.push(path.to_path_buf());
        self.persist();
    }

    /// `None` when the user has never moved the slider, which follows the palette.
    pub fn glass_opacity(&self) -> Option<u8> {
        self.filing.glass_opacity
    }

    pub fn set_glass_opacity(&mut self, opacity: u8) {
        self.filing.glass_opacity = Some(opacity);
        self.persist();
    }

    /// The accent the user picked, or the default when they never have.
    pub fn accent(&self) -> Accent {
        match self.filing.accent.as_deref() {
            Some("red") => Accent::Red,
            Some("blue") => Accent::Blue,
            // Both "teal" and anything unrecognised. A sidecar naming a colour this build
            // has never heard of is a downgrade, not a corruption.
            _ => Accent::Teal,
        }
    }

    pub fn set_accent(&mut self, accent: Accent) {
        self.filing.accent = Some(
            match accent {
                Accent::Teal => "teal",
                Accent::Red => "red",
                Accent::Blue => "blue",
            }
            .to_owned(),
        );
        self.persist();
    }

    /// The address the last sign-in used, or `None` if nobody has ever signed in.
    pub fn sync_server(&self) -> Option<&str> {
        self.filing.sync_server.as_deref()
    }

    /// The name the last sign-in used.
    pub fn sync_username(&self) -> Option<&str> {
        self.filing.sync_username.as_deref()
    }

    /// Records who signed in where, so the Account page opens filled in next time.
    ///
    /// One setter for both, because they are only ever written together — a username with no
    /// server, or a server with somebody else's name beside it, is a page that opens with two
    /// halves of two different sign-ins in it. One `persist` rather than two, for the same
    /// reason the accent slider waits for the drag to end: this writes the sidecar
    /// synchronously.
    ///
    /// **No password is written here, and there is no field for one.** See `crate::signin`.
    pub fn set_sync_account(&mut self, server: &str, username: &str) {
        self.filing.sync_server = Some(server.to_owned());
        self.filing.sync_username = Some(username.to_owned());
        self.persist();
    }

    pub fn last_board(&self) -> Option<&Path> {
        self.filing.last_board.as_deref()
    }

    pub fn set_last_board(&mut self, path: Option<&Path>) {
        self.filing.last_board = path.map(Path::to_path_buf);
        self.persist();
    }

    /// The camera a board opens at, as `[x, y, zoom]`.
    pub fn start_view(&self, path: &Path) -> Option<[f64; 3]> {
        self.filing
            .start_views
            .iter()
            .find(|view| view.board == path)
            .map(|view| view.camera)
    }

    pub fn set_start_view(&mut self, path: &Path, camera: [f64; 3]) {
        self.filing.start_views.retain(|view| view.board != path);
        self.filing
            .start_views
            .push(StartView { board: path.to_path_buf(), camera });
        self.persist();
    }

    // ----- the four verbs on a board ----------------------------------------

    /// Creates an empty board and returns the file it landed in.
    pub fn create(&mut self, title: &str) -> Result<PathBuf> {
        let title = if title.trim().is_empty() { "Untitled board" } else { title.trim() };
        let path = self.free_path(title);
        let mut board = Board::new();
        board.set_title(title).context("naming the new board")?;
        let mut db = BoardDb::open(&path).with_context(|| format!("creating {}", path.display()))?;
        db.save(&board).context("writing the new board")?;
        db.close().context("closing the new board")?;
        self.rescan();
        Ok(path)
    }

    /// Copies a board, content and all, under a new name.
    ///
    /// A byte copy of the file rather than a load-and-save: it keeps the restore
    /// points and the chunk layout, and it cannot fail halfway through a CRDT replay
    /// on a board that is only slightly damaged. The title inside the copy is then
    /// rewritten so the library does not show the same name twice.
    pub fn duplicate(&mut self, path: &Path) -> Result<PathBuf> {
        let source_title = self
            .cards
            .iter()
            .find(|c| c.path == path)
            .map_or_else(|| stem_of(path), |c| c.title.clone());
        let title = format!("{source_title} copy");
        let target = self.free_path(&title);

        // Only the main database file is copied. A `-wal` beside it holds writes the
        // *other* handle has not checkpointed, and copying it into a differently named
        // database is how a WAL gets applied to the wrong file.
        std::fs::copy(path, &target)
            .with_context(|| format!("copying {} to {}", path.display(), target.display()))?;

        let mut db = BoardDb::open(&target)?;
        if let Some(mut board) = db.load()? {
            board.set_title(&title)?;
            db.save(&board)?;
        }
        db.close()?;

        // A duplicate belongs where its original was filed. Anything else means the
        // user duplicates a board in a space and it lands somewhere else.
        let spaces: Vec<String> = self
            .filing
            .spaces
            .iter()
            .filter(|s| s.boards.iter().any(|p| p == path))
            .map(|s| s.name.clone())
            .collect();
        for name in spaces {
            self.move_to_space(&target, Some(&name));
        }

        self.rescan();
        Ok(target)
    }

    /// Renames the board. See the module header for why the file keeps its name.
    pub fn rename(&mut self, path: &Path, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() {
            anyhow::bail!("a board needs a name");
        }
        let mut db = BoardDb::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut board = db
            .load()?
            .ok_or_else(|| anyhow::anyhow!("{} holds no board", path.display()))?;
        board.set_title(title)?;
        db.save(&board)?;
        db.close()?;
        self.rescan();
        Ok(())
    }

    /// Moves a board to **Recently deleted**. Nothing on disk is touched.
    ///
    /// This is what Delete does now. The board stays in whatever folder it was filed under
    /// and stays starred if it was starred — `LibraryState::in_scope` hides it from every
    /// scope but the trash — so [`Self::restore`] puts it back exactly as it was rather
    /// than dropping it into no folder with no star.
    ///
    /// Idempotent: deleting a board already in the trash leaves the original time, so a
    /// second Delete cannot quietly reset how long it has been there.
    pub fn trash(&mut self, path: &Path) {
        if self.filing.trashed.iter().any(|t| t.path == path) {
            return;
        }
        let at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.filing.trashed.push(TrashedBoard { path: path.to_path_buf(), at });
        self.persist();
        self.rescan();
    }

    /// Takes a board back out of Recently deleted.
    pub fn restore(&mut self, path: &Path) {
        self.filing.trashed.retain(|t| t.path != path);
        self.persist();
        self.rescan();
    }

    /// Whether a board is in Recently deleted.
    pub fn is_trashed(&self, path: &Path) -> bool {
        self.filing.trashed.iter().any(|t| t.path == path)
    }

    /// Everything in Recently deleted, for Empty.
    pub fn trashed(&self) -> Vec<PathBuf> {
        self.filing.trashed.iter().map(|t| t.path.clone()).collect()
    }

    /// Deletes the board file and everything SQLite keeps beside it.
    ///
    /// **The only path in this application that removes a `.vellum`.** Reachable from
    /// Recently deleted and nowhere else, behind its own confirmation — see RULE ZERO at
    /// the top of `CLAUDE.md`. Every other Delete in the interface goes to [`Self::trash`].
    pub fn purge(&mut self, path: &Path) -> Result<()> {
        std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
        // The write-ahead log and the shared-memory file are SQLite's, and leaving
        // them behind makes the next board created at the same path inherit writes
        // that belonged to the deleted one.
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(sidecar));
        }
        self.filing.starred.retain(|p| p != path);
        self.filing.trashed.retain(|t| t.path != path);
        for space in &mut self.filing.spaces {
            space.boards.retain(|p| p != path);
        }
        self.persist();
        self.rescan();
        Ok(())
    }

    // ----- stars and spaces --------------------------------------------------

    pub fn set_starred(&mut self, path: &Path, starred: bool) {
        self.filing.starred.retain(|p| p != path);
        if starred {
            self.filing.starred.push(path.to_path_buf());
        }
        for card in &mut self.cards {
            if card.path == path {
                card.starred = starred;
            }
        }
        self.persist();
    }

    /// Files a board under a space, or takes it out of every space with `None`.
    ///
    /// A board is in at most one space, matching the user's Miro: their real folders
    /// — Personal, Books, Cars, Archive — are a filing scheme, not tags.
    pub fn move_to_space(&mut self, path: &Path, space: Option<&str>) {
        for record in &mut self.filing.spaces {
            record.boards.retain(|p| p != path);
        }
        if let Some(name) = space
            && let Some(record) = self.filing.spaces.iter_mut().find(|s| s.name == name)
        {
            record.boards.push(path.to_path_buf());
        }
        self.persist();
    }

    /// Adds a space. Returns false when one of that name already exists, which the
    /// caller reports rather than silently merging two folders.
    pub fn create_space(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.filing.spaces.iter().any(|s| s.name == name) {
            return false;
        }
        self.filing.spaces.push(SpaceRecord {
            name: name.to_owned(),
            boards: Vec::new(),
            pinned: false,
        });
        self.sort_spaces();
        self.persist();
        true
    }

    pub fn rename_space(&mut self, from: &str, to: &str) -> bool {
        let to = to.trim();
        if to.is_empty() || self.filing.spaces.iter().any(|s| s.name == to) {
            return false;
        }
        let Some(record) = self.filing.spaces.iter_mut().find(|s| s.name == from) else {
            return false;
        };
        record.name = to.to_owned();
        self.sort_spaces();
        self.persist();
        true
    }

    /// Deletes a space. **Never the boards in it** — the folder goes, the work stays.
    pub fn delete_space(&mut self, name: &str) {
        self.filing.spaces.retain(|s| s.name != name);
        self.persist();
    }

    pub fn set_space_pinned(&mut self, name: &str, pinned: bool) {
        if let Some(record) = self.filing.spaces.iter_mut().find(|s| s.name == name) {
            record.pinned = pinned;
            self.sort_spaces();
            self.persist();
        }
    }

    /// Points a board's library row at a rendered preview in the blob store.
    pub fn set_thumbnail(&mut self, path: &Path, hash: &vellum_store::Hash) -> Result<()> {
        let mut db = BoardDb::open(path)?;
        db.set_thumbnail(Some(hash))?;
        db.close()?;
        Ok(())
    }

    /// The thumbnail hash a board's row carries, if it has one.
    pub fn thumbnail_hash(&self, path: &Path) -> Option<vellum_store::Hash> {
        BoardDb::open(path).ok()?.index().ok()??.thumbnail
    }

    // ----- internals ---------------------------------------------------------

    /// Pinned first, then alphabetical. Matching the user's Miro sidebar, where the
    /// pinned spaces sit at the top.
    fn sort_spaces(&mut self) {
        self.filing.spaces.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    /// A file name derived from the title that is not already taken.
    fn free_path(&self, title: &str) -> PathBuf {
        let stem = slug(title);
        let mut candidate = self.root.join(format!("{stem}.{BOARD_EXTENSION}"));
        let mut n = 2;
        while candidate.exists() {
            candidate = self.root.join(format!("{stem}-{n}.{BOARD_EXTENSION}"));
            n += 1;
        }
        candidate
    }

    /// Writes the sidecar. Failure is logged and not propagated: losing a star is not
    /// worth failing the action the user actually asked for.
    fn persist(&self) {
        match serde_json::to_vec_pretty(&self.filing) {
            Ok(bytes) => {
                if let Err(error) = std::fs::write(&self.sidecar, bytes) {
                    log::warn!("writing {}: {error}", self.sidecar.display());
                }
            }
            Err(error) => log::warn!("encoding the library filing: {error}"),
        }
    }
}

/// One board in Recently deleted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TrashedBoard {
    path: PathBuf,
    /// Seconds since the epoch. A number rather than a `SystemTime` because that is what
    /// survives a round trip through JSON on every platform without a serde feature, and
    /// because a sidecar written by a build that spelt it differently must still parse —
    /// see the `accent` field for the same reasoning about strings.
    at: u64,
}

impl TrashedBoard {
    fn when(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(self.at)
    }
}

fn card_of(
    index: BoardIndex,
    starred: &BTreeSet<&PathBuf>,
    trashed: &BTreeMap<&PathBuf, SystemTime>,
) -> BoardCard {
    BoardCard {
        starred: starred.contains(&index.path),
        deleted: trashed.get(&index.path).copied(),
        path: index.path,
        title: index.title,
        item_count: index.item_count,
        modified: index.modified,
        // Filled in by the shell once the preview has been decoded and uploaded; the
        // library lists a board with no thumbnail perfectly well, which is what makes
        // listing a hundred of them free.
        thumbnail: None,
    }
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "board".to_owned())
}

/// A file-name stem from a board title.
///
/// Conservative on purpose: the result is a path on two operating systems with
/// different reserved characters, and a board called `Engine bay / wiring` must not
/// become a directory. Anything that is not an ASCII letter, digit or dash becomes a
/// dash, runs collapse, and an empty result falls back to a fixed name.
pub(crate) fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "board".to_owned()
    } else {
        trimmed.chars().take(64).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("boards"));
        (dir, library)
    }

    /// An attached `.rtb` has to survive a restart, or the picker is a per-session
    /// convenience and the user is back to the archives folder.
    ///
    /// This is the half no real run can show: reopening the app is what the assertion is
    /// about, and `--screenshot` photographs one launch.
    #[test]
    fn an_attached_archive_is_remembered_across_a_restart() {
        let (dir, mut library) = library();
        let rtb = dir.path().join("board.rtb");
        std::fs::write(&rtb, b"pretend archive").unwrap();

        library.attach_archive(&rtb);
        assert_eq!(library.attached_archives(), vec![rtb.clone()]);

        // Attaching the same file again is what a user does when unsure it worked the
        // first time. It must not stack up entries that then load one 114MB archive twice.
        library.attach_archive(&rtb);
        assert_eq!(library.attached_archives().len(), 1, "attaching twice is idempotent");

        // The restart. A second `Library` over the same directory reads the sidecar the
        // first one wrote — there is no in-memory state carried across.
        let reopened = Library::open(dir.path().join("boards"));
        assert_eq!(reopened.attached_archives(), vec![rtb.clone()], "the sidecar carried it");

        // A backup the user has since moved or deleted is dropped rather than reported.
        // The entry stays in the file on purpose: an unplugged volume comes back.
        std::fs::remove_file(&rtb).unwrap();
        let pruned = Library::open(dir.path().join("boards"));
        assert!(pruned.attached_archives().is_empty(), "a missing archive is skipped");
    }

    #[test]
    fn a_title_becomes_a_safe_file_stem() {
        assert_eq!(slug("Engine bay"), "engine-bay");
        assert_eq!(slug("Panel 2020 4k v3"), "panel-2020-4k-v3");
        // A separator in a title must not become a directory separator.
        assert_eq!(slug("wiring / loom"), "wiring-loom");
        assert_eq!(slug("  "), "board");
        assert_eq!(slug("…"), "board");
        assert!(!slug("///a///").contains('/'));
    }

    #[test]
    fn creating_a_board_makes_a_listable_file() {
        let (_dir, mut library) = library();
        assert!(library.cards().is_empty());

        let path = library.create("Engine bay").unwrap();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some(BOARD_EXTENSION));
        assert_eq!(library.cards().len(), 1);
        assert_eq!(library.cards()[0].title, "Engine bay");
        assert_eq!(library.cards()[0].path, path);
    }

    /// Two boards with the same name are ordinary. Overwriting the first is not.
    #[test]
    fn two_boards_of_the_same_name_get_different_files() {
        let (_dir, mut library) = library();
        let first = library.create("Notes").unwrap();
        let second = library.create("Notes").unwrap();
        assert_ne!(first, second);
        assert_eq!(library.cards().len(), 2);
    }

    #[test]
    fn duplicating_copies_the_content_and_renames_the_copy() {
        let (_dir, mut library) = library();
        let path = library.create("Wiring").unwrap();
        let copy = library.duplicate(&path).unwrap();

        assert_ne!(copy, path);
        let titles: Vec<&str> = library.cards().iter().map(|c| c.title.as_str()).collect();
        assert!(titles.contains(&"Wiring"), "{titles:?}");
        assert!(titles.contains(&"Wiring copy"), "{titles:?}");
    }

    /// A duplicate has to land in the same space as its original, or duplicating a
    /// board inside a folder quietly files it somewhere else.
    #[test]
    fn a_duplicate_inherits_its_originals_space() {
        let (_dir, mut library) = library();
        let path = library.create("Wiring").unwrap();
        assert!(library.create_space("Cars"));
        library.move_to_space(&path, Some("Cars"));

        let copy = library.duplicate(&path).unwrap();
        let cars = library.spaces().into_iter().find(|s| s.name == "Cars").unwrap();
        assert!(cars.contains(&path));
        assert!(cars.contains(&copy));
    }

    #[test]
    fn renaming_changes_the_title_and_leaves_the_file_where_it_is() {
        let (_dir, mut library) = library();
        let path = library.create("Draft").unwrap();
        library.rename(&path, "Engine bay").unwrap();

        assert!(path.exists(), "renaming must not move the file");
        assert_eq!(library.cards()[0].title, "Engine bay");
        assert_eq!(library.cards()[0].path, path);
        assert!(library.rename(&path, "   ").is_err(), "a board needs a name");
    }

    /// Delete puts a board in Recently deleted and **does not touch the file**.
    ///
    /// *"when a person deletes their board make it so that it puts it in the recently
    /// deleted folder"*, and the file staying exactly where it is is what makes the trash
    /// trustworthy: there is no move to fail halfway, no volume to cross, no name to
    /// collide with. Restoring is dropping a line from the sidecar.
    ///
    /// The filing is kept too — the star and the folder — which is the difference between
    /// *restore* and *create a new board with the same name*. This is the assertion that
    /// fails if someone later "tidies up" by clearing the filing on delete the way `purge`
    /// legitimately does.
    #[test]
    fn deleting_moves_a_board_to_the_trash_and_leaves_it_on_disk() {
        let (_dir, mut library) = library();
        let path = library.create("Scratch").unwrap();
        assert!(library.create_space("Archive"));
        library.move_to_space(&path, Some("Archive"));
        library.set_starred(&path, true);

        library.trash(&path);
        assert!(path.exists(), "the file must not be touched");
        assert!(library.is_trashed(&path));
        assert_eq!(library.trashed(), vec![path.clone()]);
        // Still listed, and now carrying the flag every scope but the trash filters on.
        assert_eq!(library.cards().len(), 1);
        assert!(library.cards()[0].deleted.is_some());
        assert!(library.is_starred(&path), "a restored board keeps its star");
        assert_eq!(library.spaces()[0].boards, vec![path.clone()], "and its folder");

        // Twice is once: a second Delete must not reset how long it has been in there.
        let first = library.cards()[0].deleted;
        library.trash(&path);
        assert_eq!(library.trashed().len(), 1);
        assert_eq!(library.cards()[0].deleted, first);

        library.restore(&path);
        assert!(!library.is_trashed(&path));
        assert!(library.cards()[0].deleted.is_none());
    }

    /// Purge is the only thing that removes a `.vellum`, and it takes the filing with it.
    #[test]
    fn purging_removes_the_file_and_the_filing_that_pointed_at_it() {
        let (_dir, mut library) = library();
        let path = library.create("Scratch").unwrap();
        assert!(library.create_space("Archive"));
        library.move_to_space(&path, Some("Archive"));
        library.set_starred(&path, true);
        library.trash(&path);

        library.purge(&path).unwrap();
        assert!(!path.exists());
        assert!(library.cards().is_empty());
        assert!(!library.is_starred(&path));
        assert!(!library.is_trashed(&path), "and it leaves the trash it was in");
        assert!(library.spaces()[0].boards.is_empty());
    }

    /// A space is a folder, not a bin. Deleting one must never take the work with it.
    #[test]
    fn deleting_a_space_keeps_its_boards() {
        let (_dir, mut library) = library();
        let path = library.create("Keyboard").unwrap();
        assert!(library.create_space("Posters"));
        library.move_to_space(&path, Some("Posters"));

        library.delete_space("Posters");
        assert!(library.spaces().is_empty());
        assert!(path.exists());
        assert_eq!(library.cards().len(), 1);
    }

    #[test]
    fn a_board_is_in_at_most_one_space() {
        let (_dir, mut library) = library();
        let path = library.create("Keyboard").unwrap();
        assert!(library.create_space("Cars"));
        assert!(library.create_space("Books"));

        library.move_to_space(&path, Some("Cars"));
        library.move_to_space(&path, Some("Books"));
        let spaces = library.spaces();
        assert_eq!(spaces.iter().filter(|s| s.contains(&path)).count(), 1);

        library.move_to_space(&path, None);
        assert!(spaces_of(&library, &path).is_empty());
    }

    fn spaces_of(library: &Library, path: &Path) -> Vec<String> {
        library
            .spaces()
            .into_iter()
            .filter(|s| s.contains(path))
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn spaces_are_pinned_first_then_alphabetical() {
        let (_dir, mut library) = library();
        for name in ["Posters", "Archive", "Cars"] {
            assert!(library.create_space(name));
        }
        library.set_space_pinned("Posters", true);

        let names: Vec<String> = library.spaces().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["Posters", "Archive", "Cars"]);
    }

    #[test]
    fn a_space_name_is_not_reused() {
        let (_dir, mut library) = library();
        assert!(library.create_space("Cars"));
        assert!(!library.create_space("Cars"));
        assert!(!library.create_space("  "));
        assert!(library.create_space("Books"));
        assert!(!library.rename_space("Books", "Cars"));
        assert!(library.rename_space("Books", "Manuals"));
    }

    /// The filing outlives the process, or spaces are a per-session toy.
    #[test]
    fn the_filing_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("boards");
        let path = {
            let mut library = Library::open(&root);
            let path = library.create("Engine bay").unwrap();
            assert!(library.create_space("Cars"));
            library.move_to_space(&path, Some("Cars"));
            library.set_starred(&path, true);
            library.set_theme_preference(ThemePreference::Dark);
            library.set_accent(Accent::Blue);
            path
        };

        let library = Library::open(&root);
        assert!(library.is_starred(&path));
        assert_eq!(library.theme_preference(), ThemePreference::Dark);
        assert_eq!(library.accent(), Accent::Blue, "the accent has to outlive the process");
        assert_eq!(library.spaces().len(), 1);
        assert!(library.spaces()[0].contains(&path));
        assert!(library.cards()[0].starred, "the card has to carry the star too");
    }

    /// A sidecar someone edited by hand, or one truncated by a power cut, must not
    /// stop the app from listing the boards.
    #[test]
    fn a_damaged_sidecar_degrades_to_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("boards");
        {
            let mut library = Library::open(&root);
            library.create("Engine bay").unwrap();
        }
        std::fs::write(root.join("library.json"), b"{ not json").unwrap();

        let library = Library::open(&root);
        assert_eq!(library.cards().len(), 1);
        assert!(library.spaces().is_empty());
        assert_eq!(library.theme_preference(), ThemePreference::System);
    }

    /// ⚠ **The archived Agent Canvas's six settings must survive an ordinary rewrite.**
    ///
    /// RULE ZERO applied to a preference file rather than to a board. The layer moved to
    /// `archive/` and nothing reads these keys any more — but this user's `library.json`
    /// already holds all six, and [`Library::persist`] writes the **whole** struct back on
    /// every star, rename, delete and theme change. Drop the fields from `Filing` and the
    /// first ordinary action erases the user's answers, silently, with every board intact so
    /// nothing looks wrong until somebody goes looking. They are kept unread precisely so
    /// that cannot happen; this is the assertion that says so.
    ///
    /// `speech` is checked *inside* rather than by key, because it is the one field whose
    /// type lived in the archived crate: a stand-in that modelled none of its contents would
    /// keep the key and write back an empty object, which passes a key-only check.
    ///
    /// A/B: delete the six fields and this fails on the first assertion.
    #[test]
    fn the_archived_agent_settings_survive_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("boards");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("library.json"),
            br#"{
                "agent_display": "raw",
                "agent_provider": "codex",
                "agent_chat_theme": "terminal",
                "browser_nodes": true,
                "worktrees": true,
                "speech": { "preference": "Local", "model_file": "/models/base.bin" }
            }"#,
        )
        .unwrap();

        // Any ordinary action rewrites the sidecar; a star is the cheapest one to reach.
        let mut library = Library::open(&root);
        let path = library.create("Engine bay").unwrap();
        library.set_starred(&path, true);

        let bytes = std::fs::read(root.join("library.json")).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(written["agent_display"].as_str(), Some("raw"));
        assert_eq!(written["agent_provider"].as_str(), Some("codex"));
        assert_eq!(written["agent_chat_theme"].as_str(), Some("terminal"));
        assert_eq!(written["browser_nodes"].as_bool(), Some(true));
        assert_eq!(written["worktrees"].as_bool(), Some(true));
        assert_eq!(
            written["speech"]["model_file"].as_str(),
            Some("/models/base.bin"),
            "`speech` kept its key and lost what was inside it"
        );
        assert!(library.is_starred(&path), "the rewrite this test relies on did happen");
    }

    /// The account is remembered, the password is not, and the archived keys still survive.
    ///
    /// Three assertions in one place because they are one property: `persist` writes the
    /// **whole** struct, so a new field is only safe if everything already on disk comes back
    /// out with it. The third assertion is the one that would catch a `Filing` where the new
    /// fields were added and an old one was dropped in the same edit.
    ///
    /// A sidecar with no `sync_server` key is the ordinary case — every existing
    /// `library.json` on this machine is one — and it has to read as `None` rather than fail
    /// the parse.
    #[test]
    fn the_account_is_remembered_and_the_password_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("boards");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("library.json"), br#"{ "worktrees": true }"#).unwrap();

        let mut library = Library::open(&root);
        assert_eq!(library.sync_server(), None, "a sidecar with no key is not a failure");
        assert_eq!(library.sync_username(), None);

        library.set_sync_account("https://boards.example.com/", "sam");
        assert_eq!(library.sync_server(), Some("https://boards.example.com/"));
        assert_eq!(library.sync_username(), Some("sam"));

        let bytes = std::fs::read(root.join("library.json")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let written: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(written["sync_server"].as_str(), Some("https://boards.example.com/"));
        assert_eq!(written["sync_username"].as_str(), Some("sam"));
        assert_eq!(written["worktrees"].as_bool(), Some(true), "an existing key was dropped");
        // ⚠ **The assertion this test exists for.** There is no key for a password, so there
        // is nothing to accidentally start writing into one.
        assert!(!text.contains("password"), "a password key appeared in the sidecar: {text}");

        // And it survives a reopen, which is the whole point of writing it.
        let reopened = Library::open(&root);
        assert_eq!(reopened.sync_server(), Some("https://boards.example.com/"));
        assert_eq!(reopened.sync_username(), Some("sam"));
    }

    /// Filing that points at a board someone deleted from the Finder has to go, or a
    /// space shows a count that never matches what it lists.
    #[test]
    fn filing_for_a_vanished_board_is_pruned() {
        let (_dir, mut library) = library();
        let path = library.create("Gone").unwrap();
        assert!(library.create_space("Cars"));
        library.move_to_space(&path, Some("Cars"));
        library.set_starred(&path, true);

        std::fs::remove_file(&path).unwrap();
        library.rescan();

        assert!(library.cards().is_empty());
        assert!(library.spaces()[0].boards.is_empty());
        assert!(!library.is_starred(&path));
    }
}
