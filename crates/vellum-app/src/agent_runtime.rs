//! The live session pool: processes, threads, channels — drained once per frame.
//!
//! This is `crate::links` for agents, and it is deliberately built to the same shape,
//! because that shape is the one this application has already proved: **blocking work on a
//! worker, answers through a channel, applied by the frame loop that owns the document**.
//! No async runtime, no second execution model, and nothing here ever blocks a frame.
//!
//! ```text
//!   a node the user started ──► Session ──► transport ──► a child process
//!            │                     │
//!            │                  poll()  ──► TranscriptEvent ──► sidecar JSONL (disk)
//!            │                                              └─► the node's event ring
//!            │
//!   IpcServer (its own thread) ──► Job + reply channel ──► drained here ──► answered
//! ```
//!
//! # The cost rule, which outranks every feature below
//!
//! `docs/07-agent-canvas.md` §0: *a board with no agent nodes must be frame-for-frame the
//! same cost as before this layer existed*. So:
//!
//! - **Nothing exists until the user starts an agent.** Placing a node costs a token in the
//!   document and nothing else. No thread, no process, no socket, no timer — the IPC server
//!   is started with the first session and stopped with the last, and the scheduler's thread
//!   does not exist while no schedule is armed.
//! - [`AgentRuntime::dormant`] is the early-out, and it is a handful of `is_empty` calls.
//!   Both per-frame entry points ([`AgentRuntime::drain`] and
//!   [`AgentRuntime::rebuild_views`]) begin with it.
//! - **Viewport culling keeps the process and drops the rendering.** An off-screen node is
//!   never given an [`crate::agent_view::AgentView`]: it is not shaped, not tessellated and
//!   not uploaded, while its transport keeps running and its transcript keeps appending.
//!   That is Maestri's mechanic and the reason dozens of agents are affordable.
//! - **Nothing here drives a repaint.** There is no spinner and no timer. Events cause the
//!   frame that draws them; a message pulse asks for repaints for the fraction of a second
//!   it is travelling and then stops, which is `landed_background`'s rule from feedback 21.
//!
//! # A transcript is never deleted, and nothing in the application deletes one
//!
//! `docs/07` §4 calls a transcript disposable, and it is — but *disposing* of one is the
//! user's act and there is no verb for it yet. [`AgentRuntime::release`] stops a session and
//! keeps the file; deleting the node keeps it; closing the board keeps it. There used to be a
//! `forget` here that removed the sidecar, written and tested and **called by nothing**, which
//! is worse than the gap: it read like the deletion path while the deletion path did not
//! exist. It is gone. When a *Clear transcript* row is added, this is the paragraph to correct
//! rather than the place to quietly re-add a method.
//!
//! # Undo groups: this module opens none, ever
//!
//! Read `CLAUDE.md` trap 11 and feedback 27/30 before touching anything here. A transcript
//! is JSONL in a sidecar and an event ring in memory — **it never enters the Loro document**
//! — so the common path needs no group at all and cannot leak one.
//!
//! Exactly two things an agent can ask for do touch the document: `spawn` and `configure`.
//! Both are **deferred** rather than served here: [`AgentRuntime::take_document_jobs`] hands
//! them to `crate::actions`, which applies them inside `Editor::edit` and **only when
//! `ActiveState::busy_with_a_group()` is false**. That is `apply_link_fetches`' rule, and the
//! reasoning is the same one: a command is something the user just asked for and closing
//! their edit to serve it is reasonable; a background arrival must never end an edit in
//! progress. Nothing is lost by waiting — the job stays queued and the agent's own request is
//! still blocked on its reply channel, so it simply takes a frame or two longer.
//!
//! # The deadlock `ipc.rs` warns about, and how it is avoided here
//!
//! [`vellum_agent::IpcServer::stop`] joins the server thread. An [`IpcHandler`] is expected to
//! block on a round trip with the frame loop — so if the frame loop calls `stop` while the
//! server thread is waiting on it, the join waits for a thread that is waiting for the caller.
//! Three things together make that unreachable, and all three are needed:
//!
//! 1. [`Handler::call`] checks a shared `stopping` flag **before parking a job**, and refuses
//!    inline when it is set. So once shutdown begins, no new request can park.
//! 2. [`AgentRuntime::shutdown`] sets that flag, then **answers every job already parked** —
//!    the inbox and the deferred queue both — with a refusal, before it touches the server.
//! 3. The handler waits with `recv_timeout`, never `recv`. A request that slipped through the
//!    window between (1) and (2) is answered by the timeout in [`REPLY_TIMEOUT`], which is
//!    well inside `ipc::CLIENT_READ_TIMEOUT`, so the shim still gets a sentence rather than a
//!    dropped connection — and the join is bounded by it rather than unbounded.
//!
//! # Ids
//!
//! A session is keyed by [`NodeKey`] — the **board** and the item, not the item alone. A
//! Loro `TreeID` is unique within one document and can collide with an unrelated item on
//! another board (the bug feedback 17 fixed for cross-board paste), and a parked board's
//! agents keep running while another tab is in front. The wire form the shim sees in
//! `VELM_AGENT_ID` is that pair, so a request names exactly one node on exactly one board.
//!
//! **Every map here that is keyed by board is keyed by board for that reason, and each one
//! has to be checked.** Two were not, and both were real: [`AgentRuntime::set_schedules`]
//! replaced the single scheduler queue with the front board's walk, so switching tabs
//! disarmed everything on the board you left; and `crate::actions`' `agent_doc` parsed the
//! item half of a key and never compared the board half, so a request could be served
//! against an unrelated item that happened to share a `TreeID` on the board in front.
//!
//! What a parked board's agents can and cannot do, stated exactly because "keep running" is
//! easy to over-read: their **processes** run, their transcripts append, their schedules stay
//! armed and fire. What waits is anything that needs the *document* — a schedule that fires
//! on a parked board is held by [`AgentRuntime::defer_due`] and runs when that board is next
//! in front, because `crate::actions` only ever holds one board. Closing the tab rather than
//! parking it is different again: [`AgentRuntime::forget_board`] stops that board's agents,
//! since nothing is left that could stop them by hand.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vellum_agent::ipc::{IpcHandler, IpcServer, NoteEntry, RuntimeFile, SpawnRequest};
use vellum_agent::sidecar::Appender;
use vellum_agent::{
    AgentError, AgentModel, BoardKey, Bus, Choice, DisplayMode, LinkDirection, Message, NoteScope,
    NoteStore, RequestId, Requester, RoleKind, Schedule, Session, Sidecar, Status, Timestamp,
    Topology, TranscriptEvent,
};
use vellum_store::BlobStore;

/// How many transcript events one node keeps in memory.
///
/// The ring is what the painter reads; the JSONL on disk is the history. 200 is far more
/// than fits in a node at any zoom and small enough that a hundred agents cost a few
/// megabytes rather than the whole session's output.
pub const TAIL_EVENTS: usize = 200;

/// How many events one node's view carries into a frame, after the display mode has filtered
/// them.
///
/// Bounded here rather than in the painter, for `agent_view.rs`'s reason: the expensive
/// decision belongs in one place, and a painter that bounded for itself would be a second
/// opinion about what a node shows.
pub const VIEW_EVENTS: usize = 40;

/// How long an IPC verb waits for the frame loop to answer it.
///
/// Finite, and that is the point — see the module header on the deadlock. Well inside
/// `vellum_agent::ipc::CLIENT_READ_TIMEOUT` (10s), so a request that times out here still
/// reaches the shim as a refusal rather than as a dropped connection.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// How many document-touching jobs may wait for a frame that is not mid-gesture.
///
/// Bounded because the queue is fed by agents and drained by the user putting their pen
/// down: an orchestrator in a loop can ask faster than a person types. Refused with a
/// sentence rather than dropped, which is `Session::MAX_QUEUED`'s rule.
const MAX_DEFERRED: usize = 64;

/// How often a *visible* note node's file is stat'd.
///
/// §8's *"a low-frequency poll while it is visible"*. Two seconds, which is far below the
/// rate at which a person notices a file has changed and far above the frame rate — a
/// per-frame `stat` for every note on screen is a syscall storm for an answer that changes
/// when somebody saves in another editor.
pub const NOTE_POLL_SECONDS: u64 = 2;

/// How long the window must have been unfocused before coming back raises a digest.
///
/// A minute, because alt-tabbing to a browser and straight back is not "away", and a digest
/// that appears every time the window loses focus is a notification nobody reads.
pub const AWAY_THRESHOLD: u64 = 60;

/// Seconds since the Unix epoch.
///
/// The one place in `vellum-app` that reads the clock for the agent layer. `vellum-agent`
/// deliberately never reads it — every function there takes the time as a parameter — so
/// that "does a daily 18:00 job fire at 17:59" is arithmetic rather than a wait.
/// The machine's offset from UTC, in seconds.
///
/// The scheduler works entirely in UTC seconds and `vellum_agent::schedule` holds no timezone
/// database on purpose — *"every day at 6 PM"* means six in the **user's** evening, and the
/// offset is the one fact that turns one into the other. Supplied by the app because this is
/// the layer that is allowed to ask the operating system what time it is.
///
/// Derived by asking for the same instant in both frames rather than by reading a timezone
/// name: it needs no database, it is correct across a daylight-saving change the moment the
/// system clock is, and it cannot be wrong about a half-hour zone.
pub fn local_utc_offset() -> i32 {
    // `%z` is the offset as `+HHMM`. Parsed rather than trusted as a number, because `+0530`
    // is five and a half hours and reading it as an integer gives 530.
    let Ok(output) = std::process::Command::new("date").arg("+%z").output() else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    let sign = if text.starts_with('-') { -1 } else { 1 };
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let Some(hours) = digits.get(..2).and_then(|h| h.parse::<i32>().ok()) else { return 0 };
    let minutes = digits.get(2..4).and_then(|m| m.parse::<i32>().ok()).unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

pub fn unix_now() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// ---------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------

/// Which agent node, on which board.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeKey {
    pub board: BoardKey,
    pub item: String,
}

impl NodeKey {
    pub fn new(board: &BoardKey, item: impl Into<String>) -> Self {
        Self { board: board.clone(), item: item.into() }
    }

    /// The form an agent's own process sees, in `VELM_AGENT_ID`.
    ///
    /// `<board-key>:<item-id>`. A board key is sixteen hex digits and a Loro `TreeID` renders
    /// as `counter@peer`, so neither half can contain the separator — which is what makes
    /// [`NodeKey::from_wire`] exact rather than a guess.
    pub fn wire(&self) -> String {
        format!("{}:{}", self.board, self.item)
    }

    /// The inverse. `None` for anything that is not a node id this build wrote.
    pub fn from_wire(wire: &str) -> Option<Self> {
        let (board, item) = wire.split_once(':')?;
        if board.is_empty() || item.is_empty() {
            return None;
        }
        Some(Self { board: BoardKey::from_raw(board), item: item.to_owned() })
    }
}

/// What the board on screen was, the last time its agent wiring was derived.
///
/// # The gate that keeps this layer free, and its exact honest cost
///
/// The wiring — which nodes are agents, which connectors join them, what each one's role and
/// schedule are — is a walk of the projection, and it must not happen once a frame. But the
/// obvious gate, *"the projection generation moved"*, fires on **every keystroke**:
/// `Projection::refresh_item` bumps it, and that fast path exists precisely so that typing
/// does not cost a rebuild.
///
/// So the rule has three parts, and [`BoardStamp::needs_resync`] is the whole of it:
///
/// - **A different board** always resyncs.
/// - **A different item count** always resyncs, because an agent node can only appear or
///   disappear by one — that is the case a board with no agents on it has to catch, and it
///   costs an `usize` comparison per frame rather than a walk.
/// - **The generation moved** resyncs *only on a board that already has agent nodes*, where
///   a node's role, its schedule or a connector's arrowhead can change without the count
///   moving. Measured against what that board is already doing on the same event, this is
///   one pass over an already-materialised vector matching on a discriminant.
///
/// The consequence, stated so it can be checked: **a board with no agent nodes pays one
/// `usize` comparison and one `Option<&Path>` comparison per frame, and nothing else.**
#[derive(Debug, Clone)]
pub struct BoardStamp {
    /// The board file this key was derived from. `None` for a board with no file yet.
    pub path: Option<PathBuf>,
    /// Where its transcripts live.
    pub key: BoardKey,
    /// The projection generation the wiring was derived at.
    pub epoch: u64,
    /// How many items the board held then.
    pub items: usize,
    /// Whether it had any **agent** node at all. Drives both the wiring resync and whether
    /// the painter is handed any view.
    pub has_nodes: bool,
    /// Whether it had any **note** node. Separate from `has_nodes` because a board can have
    /// notes and no agents — a set of file-backed markdown documents on a canvas is a
    /// perfectly ordinary thing to want — and that board still has to poll its files while
    /// costing nothing to the boards that have neither.
    pub has_notes: bool,
    /// Whether it had **any** of the four Agent Canvas kinds on it.
    ///
    /// ⚠ A third flag rather than a re-reading of the other two, because both of those are
    /// about one kind and this is about the family. `rebuild_agent_views` gated on `has_nodes`
    /// — agents alone — so a board holding only a file tree and a browser node was never given
    /// a view at all: its tree drew *"This tree has not been read yet"* for ever, and its
    /// browser drew *"Browser nodes are off"* whatever the preference said, because the flag
    /// that carries the preference is set in the same pass.
    pub has_content: bool,
}

impl BoardStamp {
    /// Whether the wiring has to be derived again. See the type's own note.
    pub fn needs_resync(&self, path: Option<&Path>, epoch: u64, items: usize) -> bool {
        self.path.as_deref() != path
            || self.items != items
            || (self.has_content && self.epoch != epoch)
    }
}

// ---------------------------------------------------------------------------------------
// What the frame loop has to do for an agent
// ---------------------------------------------------------------------------------------

/// One thing an agent asked for that only the thread owning the document can do.
///
/// Two verbs reach here — `spawn` and `configure` — and nothing else. Everything an agent can
/// ask that touches only files, the blob store or the transcript is served in
/// [`AgentRuntime::drain`] on the spot.
#[derive(Debug)]
pub enum DocumentWork {
    /// Create a sub-agent. The role check has already been made by `ipc::dispatch`; the cap
    /// and the territory have **not** — `vellum_agent::orchestrator` owns those and
    /// `crate::actions` asks it.
    Spawn { parent: NodeKey, request: SpawnRequest },
    /// Read a node's configuration. Meta agent only, enforced by `ipc::dispatch`.
    ReadConfig { node: NodeKey },
    /// Replace a node's configuration. Meta agent only.
    WriteConfig { node: NodeKey, model: Box<AgentModel> },
}

/// A deferred job and the channel its answer goes back down.
///
/// Two fields rather than one opaque type so a caller can `let Pending { work, reply } = …`,
/// match on the work and still answer afterwards — the alternative, matching on
/// `&pending.work` and calling a method on `pending` inside the arm, holds a borrow of the
/// thing it is trying to consume.
#[derive(Debug)]
pub struct Pending {
    pub work: DocumentWork,
    pub reply: Answering,
}

/// The channel one agent request is waiting on.
///
/// Every method consumes it, because a job answered twice is a protocol error and the type
/// system is a better place to say so than a comment. Dropping it without answering is also
/// correct and is what shutdown relies on — the handler's `recv_timeout` reports
/// `Disconnected`, which becomes *"Velm stopped serving that request"* rather than a hang.
#[derive(Debug)]
pub struct Answering(Sender<vellum_agent::Result<Reply>>);

impl Answering {
    pub fn answer(self, result: vellum_agent::Result<Reply>) {
        let _ = self.0.send(result);
    }

    pub fn done(self) {
        self.answer(Ok(Reply::Done));
    }

    pub fn spawned(self, agent: &NodeKey) {
        self.answer(Ok(Reply::Spawned(agent.wire())));
    }

    pub fn config(self, model: AgentModel) {
        self.answer(Ok(Reply::Config(Box::new(model))));
    }

    pub fn refuse(self, message: impl Into<String>) {
        self.answer(Err(AgentError::Refused(message.into())));
    }
}

/// What a verb answers with, in this crate's own vocabulary.
///
/// Its own enum rather than `ipc::Answer` because the handler is what translates: a job is
/// posted by one of eight trait methods with eight different return types, and one enum in
/// the middle is what keeps the channel a single type.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Done,
    Text(String),
    Notes(Vec<NoteEntry>),
    Spawned(String),
    Config(Box<AgentModel>),
}

/// One request from an agent's own process, on its way to the frame loop.
struct Job {
    /// The wire id the caller claimed. Already checked to name a real node by
    /// [`IpcHandler::role_of`], which `ipc::dispatch` calls before any verb runs.
    agent: String,
    call: Call,
    reply: Sender<vellum_agent::Result<Reply>>,
}

/// The verb, in the shape the runtime wants it.
enum Call {
    Send { to: String, text: String },
    NoteRead { path: String },
    NoteWrite { path: String, text: String, append: bool },
    NoteList,
    Spawn(Box<SpawnRequest>),
    Image { bytes: Vec<u8>, caption: Option<String> },
    Options { prompt: String, choices: Vec<Choice> },
    ReadConfig { node: String },
    WriteConfig { node: String, model: Box<AgentModel> },
}

// ---------------------------------------------------------------------------------------
// Per-node state
// ---------------------------------------------------------------------------------------

/// What is known about one agent node, whether or not it is running.
///
/// Held for a node that has *never* run too: a board reopened tomorrow shows yesterday's
/// transcript, and the whole point of the sidecar is that it survives the process.
#[derive(Default)]
struct NodeState {
    /// The tail, oldest first. Seeded from disk the first time the node is looked at and
    /// extended by [`AgentRuntime::drain`] — never re-read per frame, which would be a file
    /// read per agent per frame on a board of twenty.
    ///
    /// **`Arc`, because [`AgentRuntime::rebuild_views`] runs once a frame per visible node.**
    /// A `TranscriptEvent::Text` carries the agent's whole answer and an `Image` its caption,
    /// so handing the painter forty of them used to be forty deep string clones per node per
    /// frame — the cost this layer's own header forbids. Sharing the events costs one atomic
    /// increment each and the painter never mutates one.
    events: VecDeque<Arc<TranscriptEvent>>,
    /// True when there is more history on disk than this ring carries.
    truncated: bool,
    /// Whether the ring has been seeded from the sidecar yet.
    loaded: bool,
    /// What the user has typed into the node's prompt row and not sent.
    ///
    /// Here rather than on the caret because it must survive clicking away from the node —
    /// losing a half-written instruction to a stray click is the kind of small betrayal that
    /// stops people trusting a tool.
    draft: String,
    /// The line to show when there is no live session: the last thing that went wrong,
    /// mostly, so a node that failed and was released still says why.
    resting: String,
    /// The transcript, held open across a run of appends.
    ///
    /// `Sidecar::append` opens and closes the file per event, which is right for the
    /// occasional one and wrong for a turn that produces five hundred — `session.rs` says so
    /// itself. Opened lazily on the first write, so a node that only ever *reads* its
    /// transcript costs no file handle.
    appender: Option<Appender>,
}

/// One running agent, and the bookkeeping the session itself does not do.
struct Live {
    session: Session,
    /// The hop count this agent must carry into anything it sends while answering.
    ///
    /// **Not zero.** `bus.rs` is explicit that a message sent while handling another one
    /// carries that one's count forward, and that reaching for `0` here is how a ping-pong
    /// loop escapes its bound while every unit test passes. Set when a `Message` is handed to
    /// the session and cleared when the turn ends.
    hops: u32,
}

/// The agent-family nodes and the wires between them on one board, as of that board's last
/// epoch.
///
/// Kept per board rather than rebuilt globally because a connector lives inside one document
/// — two agents on different boards are not joined by anything — and because a parked board's
/// agents keep running and keep needing a topology.
#[derive(Default, Clone)]
struct BoardWiring {
    /// `(wire id, label, accepts messages)`.
    nodes: Vec<(String, String, bool)>,
    /// `(start wire id, end wire id, direction)`.
    links: Vec<(String, String, LinkDirection)>,
}

// ---------------------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------------------

/// Everything running, and everything that has run.
pub struct AgentRuntime {
    data_dir: PathBuf,
    blobs: BlobStore,
    sidecar: Sidecar,

    sessions: HashMap<NodeKey, Live>,
    nodes: HashMap<NodeKey, NodeState>,

    /// Per board, so a tab switch does not lose the wiring of the board it left.
    ///
    /// *When* this is re-derived is the caller's question, not this type's:
    /// [`BoardStamp::needs_resync`] is the whole of that gate and there is deliberately no
    /// second copy of the epoch here to disagree with it.
    wiring: HashMap<BoardKey, BoardWiring>,
    /// Where each board's notes live. Set when a board is registered.
    stores: HashMap<BoardKey, NoteStore>,

    bus: Bus,
    /// Every node's role, by wire id, for [`IpcHandler::role_of`] — which is called on
    /// **every** request, before the verb runs. Answered from a snapshot rather than by a
    /// round trip with the frame loop: a round trip per verb doubles the latency of every
    /// call to answer a question that changes when the user edits a node, and the token
    /// already authenticates the process tree rather than the individual node
    /// (`ipc.rs`'s own header says so), so stale-by-a-frame changes nothing.
    roles: Arc<RwLock<HashMap<String, RoleKind>>>,

    ipc: Option<IpcServer>,
    inbox: Receiver<Job>,
    post: Sender<Job>,
    stopping: Arc<AtomicBool>,
    /// Jobs that need the document, waiting for a frame that is not mid-gesture.
    deferred: VecDeque<Pending>,

    scheduler: Option<Scheduler>,
    /// Every board's armed schedules, by board.
    ///
    /// **Per board for `set_wiring`'s reason, and it was a real bug that it was not.** The
    /// scheduler holds one queue, and replacing the whole of it with the *front* board's walk
    /// disarmed every schedule on every parked board the moment a tab was switched — so an
    /// agent set to run at six in the evening simply did not, and nothing said so.
    /// `BTreeMap`, so the union below is assembled in a stable order and a re-arm that changes
    /// nothing produces a byte-identical queue.
    schedules: BTreeMap<BoardKey, Vec<(Timestamp, NodeKey)>>,
    /// Schedules that fired and have not been run yet.
    due: Vec<NodeKey>,
    /// When each schedule last fired, and whether that run failed.
    ///
    /// ⚠ Here rather than in the document, for [`AgentRuntime::note_models`]' reason. These
    /// two fields used to be written back into the node's token after every fire, through
    /// `Editor::edit` — which **records an undo step**, so `⌘Z` after a scheduled run undid a
    /// timestamp rather than whatever the user had just done. Worse, undoing it restored an
    /// *older* `last_run`, which made the schedule past due again, which re-armed it and ran
    /// the agent a second time.
    ///
    /// The cost, stated rather than hidden: **a run is remembered for the session and not
    /// across a restart.** Nothing fires early because of it — `Schedule::next_fire` with no
    /// `last_run` computes the next occurrence after *now* — and a `FilesChanged` trigger
    /// falls back to "anything newer than the epoch", which is the direction
    /// `agent_trigger_holds` already chooses deliberately: a wasted turn beats a schedule
    /// that silently never fires.
    schedule_runs: HashMap<NodeKey, (Option<Timestamp>, bool)>,
    /// What to do when the turn a schedule started ends. See [`AgentRuntime::complete`].
    completions: HashMap<NodeKey, vellum_agent::Completion>,
    /// What a completed scheduled run asked to be put in front of the user.
    reports: Vec<(NodeKey, String)>,

    /// `pending:N` → the BLAKE3 hash the bytes were stored under.
    ///
    /// Across frames, and that is deliberate: `transport::park_blob`'s counter is
    /// process-wide precisely so a map like this one cannot resolve this turn's picture to
    /// the last one's.
    blob_names: HashMap<String, String>,
    /// Placeholders whose bytes were offered and would not store.
    ///
    /// Kept apart from [`AgentRuntime::blob_names`] so that "this picture failed" is a
    /// different statement from "this picture has not arrived". An event naming one of these
    /// is written into the transcript as an **error** rather than as a picture — see
    /// [`resolve_blob`]. Without it the placeholder itself was written to the JSONL, where it
    /// stayed across restarts as a picture that resolves to nothing.
    failed_blobs: HashSet<String>,

    /// Each note node's file contents, as last read from disk.
    ///
    /// **The note's text is not in the document** — that is the whole of §8 — so it has to
    /// live somewhere the painter can be handed it from, and this is that place. Keyed like
    /// everything else here so a note on a parked board keeps what was read.
    note_text: HashMap<NodeKey, String>,
    /// Each note node's model **as the poll last left it** — its stamp and its derived links.
    ///
    /// ⚠ Here rather than in the document, and that is a fix rather than a convenience. The
    /// poll used to write the stamp back into the node's token every time a file changed,
    /// through `Editor::edit` — which **records an undo step**. A note an agent writes every
    /// two seconds therefore put an mtime stamp on the undo stack twice a second, so `⌘Z`
    /// undid a timestamp instead of the user's last action, over and over.
    ///
    /// A stamp is a cache of "what this process last read", not board content: it is worth
    /// exactly one re-read to reconstruct, which is what a reopened board now does. Keeping it
    /// out of the document is also the RULE ZERO posture — a background poll that writes to a
    /// `.vellum` on a timer is a risk taken for a value nobody would miss.
    note_models: HashMap<NodeKey, vellum_agent::NoteModel>,
    /// When each note was last stat'd, so the freshness check is a low-frequency poll rather
    /// than a syscall per note per frame.
    note_checked: HashMap<NodeKey, Timestamp>,

    /// When the window last lost focus, for the away-mode digest.
    away_since: Option<Timestamp>,
}

impl AgentRuntime {
    /// A runtime with nothing running.
    ///
    /// Costs two empty maps and a channel. **No thread, no socket, no process** — see the
    /// module header; the whole layer is free until the user starts an agent.
    pub fn new(data_dir: impl Into<PathBuf>, blobs: BlobStore) -> Self {
        let data_dir = data_dir.into();
        let (post, inbox) = channel();
        Self {
            sidecar: Sidecar::new(&data_dir),
            data_dir,
            blobs,
            sessions: HashMap::new(),
            nodes: HashMap::new(),
            wiring: HashMap::new(),
            stores: HashMap::new(),
            bus: Bus::new(),
            roles: Arc::new(RwLock::new(HashMap::new())),
            ipc: None,
            inbox,
            post,
            stopping: Arc::new(AtomicBool::new(false)),
            deferred: VecDeque::new(),
            scheduler: None,
            schedules: BTreeMap::new(),
            schedule_runs: HashMap::new(),
            due: Vec::new(),
            completions: HashMap::new(),
            reports: Vec::new(),
            blob_names: HashMap::new(),
            failed_blobs: HashSet::new(),
            note_text: HashMap::new(),
            note_models: HashMap::new(),
            note_checked: HashMap::new(),
            away_since: None,
        }
    }

    /// Whether there is nothing at all to do this frame.
    ///
    /// **The property the whole layer rests on.** A board with no agent nodes reaches this
    /// and returns, so the cost of the Agent Canvas on an ordinary board is these six
    /// length checks. If this ever stops being cheap, every board in the application pays
    /// for a feature it is not using.
    pub fn dormant(&self) -> bool {
        self.sessions.is_empty()
            && self.deferred.is_empty()
            && self.due.is_empty()
            && self.reports.is_empty()
            && self.ipc.is_none()
            && self.bus.pending() == 0
    }

    /// Whether this node has a session in flight.
    pub fn is_running(&self, key: &NodeKey) -> bool {
        self.sessions.contains_key(key)
    }

    /// This node's status, or [`Status::Idle`] for one that is not running.
    pub fn status(&self, key: &NodeKey) -> Status {
        self.sessions.get(key).map_or(Status::Idle, |live| live.session.status())
    }

    /// The sidecar, for a caller that wants the history rather than the tail — the away-mode
    /// digest, and the fixtures that script a transcript through the real store.
    pub fn sidecar(&self) -> &Sidecar {
        &self.sidecar
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    // ----- boards ------------------------------------------------------------------

    /// Tell the runtime where a board's notes live.
    ///
    /// `project` is the board file's own directory when it is inside one, and `None`
    /// otherwise — §8's *"a board with no project directory still gets notes"*, at
    /// `<data-dir>/agents/<board-key>/notes/`. A note must always have somewhere to live.
    pub fn register_board(&mut self, board: &BoardKey, project: Option<&Path>) {
        let store = NoteStore::locate(project, &self.data_dir, board.as_str());
        self.stores.insert(board.clone(), store);
    }

    /// The note store for a board, if it has been registered.
    pub fn note_store(&self, board: &BoardKey) -> Option<&NoteStore> {
        self.stores.get(board)
    }

    /// Replace one board's wiring and re-derive everything that depends on it.
    ///
    /// Called when that board's projection epoch changes, never per frame: walking every
    /// connector on the board once a frame is a cost that scales with the board rather than
    /// with what is on screen, which is exactly the property this renderer exists to have.
    pub fn set_wiring(
        &mut self,
        board: &BoardKey,
        nodes: Vec<(String, String, bool)>,
        links: Vec<(String, String, LinkDirection)>,
        roles: Vec<(String, RoleKind)>,
    ) {
        self.wiring.insert(board.clone(), BoardWiring { nodes, links });
        self.rebuild_topology();

        // The roles snapshot is replaced for *this* board's ids and left alone for every
        // other one, so registering a board does not blind the server to the boards behind
        // the other tabs.
        if let Ok(mut table) = self.roles.write() {
            let prefix = format!("{board}:");
            table.retain(|id, _| !id.starts_with(&prefix));
            for (id, role) in roles {
                table.insert(id, role);
            }
        }

        // **A node the user deleted takes its session with it**, and this is the only place
        // that can notice: the wiring is derived from the document, so a node that is gone is
        // simply absent from the list above. Without this a deleted running agent keeps its
        // process forever — `dormant()` never becomes true again, the loopback server never
        // comes down, and the Stop button went with the node, so there is no way left to stop
        // it at all.
        //
        // **Released, never deleted.** Releasing stops the process and keeps the transcript.
        // A node can be absent because the user pressed ⌘Z on the paste that made it, and an
        // undo that silently destroyed the history is exactly the kind of thing RULE ZERO's
        // posture is against — so nothing here removes a sidecar, and see the module header
        // on why no verb in the application does either.
        //
        // Scoped to this board's ids, so an agent running on a board behind another tab is
        // untouched — which is the whole reason a session is keyed by board and item.
        // Owned rather than borrowed from `self.wiring`: the loop below takes `&mut self`,
        // and one allocation per resync of a board that has agents on it is cheaper than
        // relying on exactly where a borrow is considered to end.
        let known: HashSet<String> = self
            .wiring
            .get(board)
            .map(|wiring| wiring.nodes.iter().map(|(id, _, _)| id.clone()).collect())
            .unwrap_or_default();
        let orphaned: Vec<NodeKey> = self
            .sessions
            .keys()
            .filter(|key| &key.board == board && !known.contains(&key.wire()))
            .cloned()
            .collect();
        for key in orphaned {
            log::info!("agents: releasing {} — its node is no longer on the board", key.item);
            self.release(&key);
        }
    }

    /// Compose every board's wiring into the one topology the bus routes against.
    ///
    /// One topology rather than one per board, because [`Bus`] holds one — and composing is
    /// legitimate because the ids are board-qualified, so two boards cannot collide and a
    /// message can never cross from one to the other (there is no connector that could carry
    /// it).
    fn rebuild_topology(&mut self) {
        let mut topology = Topology::new();
        for wiring in self.wiring.values() {
            for (id, label, accepts) in &wiring.nodes {
                topology.node(id.clone(), label.clone(), *accepts);
            }
        }
        // Links after nodes, in a second pass over the same set: `Topology::link` records an
        // edge whether or not the endpoints are registered, and an edge to an id that is
        // never registered would be a target `Bus::send` refuses with the wrong sentence.
        for wiring in self.wiring.values() {
            for (start, end, direction) in &wiring.links {
                topology.link(start, end, *direction);
            }
        }
        self.bus.set_topology(topology);
    }

    // ----- starting and stopping ---------------------------------------------------

    /// Start a session for this node, or answer why not.
    ///
    /// **Probes the command first**, so a machine with no `claude` on its `PATH` gets
    /// *"`claude` is not installed or is not on your PATH"* — the one sentence with a remedy
    /// in it — rather than a spawn failure that says `NotFound` and names nothing. Skipped
    /// for HTTP, which has no binary to look for: `resolved_command()` answering `None` is
    /// *"this provider has no CLI to delegate to"*, which is a different statement from
    /// "the binary is missing".
    pub fn start(
        &mut self,
        key: &NodeKey,
        spec: vellum_agent::LaunchSpec,
    ) -> vellum_agent::Result<()> {
        if self.sessions.contains_key(key) {
            return Ok(());
        }
        if spec.provider.effective_transport() != vellum_agent::Transport::Http {
            match spec.resolved_command() {
                Some(command) => {
                    vellum_agent::transport::probe_command(command)?;
                }
                None => {
                    return Err(AgentError::Refused(
                        "this agent has no command to run — choose a provider in the inspector"
                            .into(),
                    ));
                }
            }
        }

        // The server exists exactly while a session does. Started here rather than when a
        // node is placed, because a board of agents nobody has run must open no socket.
        self.ensure_ipc();

        let session = Session::start(&spec)?;
        self.sessions.insert(key.clone(), Live { session, hops: 0 });
        Ok(())
    }

    /// Ask a running agent something. Queues behind the turn in flight; see `session.rs`.
    pub fn prompt(&mut self, key: &NodeKey, text: &str) -> vellum_agent::Result<()> {
        match self.sessions.get_mut(key) {
            Some(live) => live.session.prompt(text),
            None => Err(AgentError::Refused("that agent is not running".into())),
        }
    }

    /// Stop the turn in flight. Harmless when there is none.
    pub fn cancel(&mut self, key: &NodeKey) -> vellum_agent::Result<()> {
        match self.sessions.get_mut(key) {
            Some(live) => live.session.cancel(),
            None => Ok(()),
        }
    }

    /// Answer a permission request.
    ///
    /// Which requests are outstanding is **not** exposed here and deliberately is not: the
    /// painter learns about one from the `PermissionAsked` event in the node's own ring, and
    /// a second answer to "what is this node blocked on" is a second thing that can disagree
    /// with the transcript the user is reading.
    pub fn answer_permission(
        &mut self,
        key: &NodeKey,
        id: &RequestId,
        allowed: bool,
    ) -> vellum_agent::Result<()> {
        match self.sessions.get_mut(key) {
            Some(live) => live.session.answer_permission(id, allowed),
            None => Ok(()),
        }
    }

    /// End a session and release its process. The transcript and the ring both survive: a
    /// released agent is one that has stopped, not one that never ran.
    pub fn release(&mut self, key: &NodeKey) {
        if let Some(mut live) = self.sessions.remove(key) {
            let _ = live.session.shutdown();
        }
        // The file handle goes with the session. The ring stays, so the node still draws.
        if let Some(state) = self.nodes.get_mut(key) {
            state.appender = None;
        }
        self.settle_ipc();
    }

    /// Start the loopback server if it is not already up.
    fn ensure_ipc(&mut self) {
        if self.ipc.is_some() {
            return;
        }
        let handler = Arc::new(Handler {
            post: Mutex::new(self.post.clone()),
            roles: Arc::clone(&self.roles),
            stopping: Arc::clone(&self.stopping),
        });
        match IpcServer::start(&self.data_dir, handler) {
            Ok(server) => {
                log::info!("agents: loopback server on port {}", server.port());
                self.ipc = Some(server);
            }
            // Not fatal: an agent still runs, it just cannot call back. Said once, with the
            // consequence named, rather than silently leaving every `velm-agent-cli` call to
            // fail against nothing.
            Err(error) => log::warn!(
                "agents: the loopback server would not start ({error}); \
                 agents will run but cannot message each other or read notes"
            ),
        }
    }

    /// Stop the server once the last session is gone.
    ///
    /// The same order the full shutdown uses, and for the same reason: a request that parked
    /// between the last session exiting and this call would otherwise block the `join` for
    /// [`REPLY_TIMEOUT`] — not a deadlock, because the handler waits with `recv_timeout`, but
    /// a two-second hitch on the frame loop, which is worse than any frame this application
    /// is allowed to drop. Answering what is parked first makes the join immediate.
    fn settle_ipc(&mut self) {
        if !self.sessions.is_empty() {
            return;
        }
        let Some(mut server) = self.ipc.take() else { return };
        while let Ok(job) = self.inbox.try_recv() {
            let _ = job.reply.send(Err(AgentError::Refused(
                "the agent that asked is no longer running".into(),
            )));
        }
        server.stop();
    }

    /// Where the runtime file lives, for a child's `VELM_IPC`.
    pub fn runtime_file(&self) -> PathBuf {
        RuntimeFile::path_in(&self.data_dir)
    }

    // ----- the drain ---------------------------------------------------------------

    /// Everything that has arrived since the last frame, applied.
    ///
    /// Called once per frame **before the occlusion guard** — see `app.rs`. That placement is
    /// deliberate and it is not about drawing: an occluded window is exactly when agents run
    /// longest unattended, and events that piled up in a channel instead of reaching disk
    /// would grow without bound *and* leave the away-mode digest with nothing to read. It is
    /// also why `about_to_wait`'s once-a-second occluded tick calls it.
    ///
    /// Opens no undo group and touches no document. See the module header.
    pub fn drain(&mut self, now: Timestamp) {
        if self.dormant() {
            return;
        }
        let now_ms = now.saturating_mul(1_000);

        self.drain_sessions(now, now_ms);
        self.drain_bus(now, now_ms);
        self.drain_jobs(now, now_ms);
        self.collect_due();
        self.bus.expire(now_ms);
    }

    /// Poll every live session: **events into a snapshot, then blobs, then substitution.**
    ///
    /// ⚠ This order is the whole of a defect and the previous comment here argued for the
    /// wrong one. The transport **parks the bytes and then sends the event**, so:
    ///
    /// - Taking the blobs *first* leaves a window between the two calls exactly one park
    ///   wide. Bytes parked in it are not in `blob_names` when the event that names them is
    ///   substituted, so `"blob":"pending:3"` is written into the JSONL — where it stays,
    ///   across restarts, as a picture that resolves to nothing. Nothing ever repairs it:
    ///   `record` appends and the ring is seeded from the file.
    /// - Taking the events first closes it, because park-happens-before-send makes
    ///   "the event is visible" imply "its bytes are already parked". So a blob taken *after*
    ///   an event was seen is guaranteed to include that event's bytes.
    ///
    /// Substitution therefore has to happen after both — see [`resolve_blob`], which is where
    /// a picture that would not store becomes an error rather than a broken placeholder.
    fn drain_sessions(&mut self, now: Timestamp, now_ms: u64) {
        let mut arrivals: Vec<(NodeKey, TranscriptEvent)> = Vec::new();
        let mut finished: Vec<NodeKey> = Vec::new();
        let mut turns_ended: Vec<NodeKey> = Vec::new();

        {
            let Self { sessions, blobs, blob_names, failed_blobs, .. } = self;
            for (key, live) in sessions.iter_mut() {
                // The snapshot, taken **before** the blobs. Held rather than recorded here
                // because the bytes their placeholders name have not been stored yet.
                let polled: Vec<TranscriptEvent> = live.session.poll();
                for blob in live.session.take_pending_blobs() {
                    match blobs.put(&blob.bytes) {
                        Ok(hash) => {
                            blob_names.insert(blob.id, hash.to_hex().to_string());
                        }
                        Err(error) => {
                            log::warn!("agents: storing a picture failed ({error})");
                            failed_blobs.insert(blob.id);
                        }
                    }
                }
                for mut event in polled {
                    resolve_blob(&mut event, blob_names, failed_blobs);
                    // A turn ending is where a borrowed hop count stops applying: anything
                    // this agent sends after it is something it decided to do, which is hop
                    // zero by `bus.rs`'s definition.
                    if matches!(event, TranscriptEvent::TurnEnded { .. }) {
                        live.hops = 0;
                        turns_ended.push(key.clone());
                    }
                    arrivals.push((key.clone(), event));
                }
                if !live.session.is_alive() && live.session.status() != Status::Running {
                    finished.push(key.clone());
                }
            }
        }

        for (key, event) in arrivals {
            self.record(&key, now, &event);
        }
        // **After** the events are recorded, because a completion reads the turn's own last
        // words out of the ring — applying it first would report the turn before it.
        for key in turns_ended {
            self.complete(&key, now, now_ms);
        }
        // A child that exited is released, so its process is reaped and the server can come
        // down with the last one. The transcript stays: it is the record of what it did.
        for key in finished {
            self.release(&key);
        }
    }

    /// Route what the bus has for us: one transcript line per end, and the receiving
    /// session's next turn.
    fn drain_bus(&mut self, now: Timestamp, _now_ms: u64) {
        // ⚠ **What the bus threw away, said out loud.** `Bus::MAX_QUEUED` is a memory bound and
        // it drops the *oldest* delivery — but a delivery is not a line of scrollback, it is
        // the prompt the receiving agent was about to be given, and `Bus::send` has already
        // answered its caller `Ok(())` by the time this happens. Silently, that is an
        // instruction accepted and never delivered: indistinguishable, on the board, from an
        // agent that was asked and ignored it. Taken rather than read, so one loss is one
        // sentence and not one per frame for the rest of the session.
        for (node, lost) in self.bus.take_dropped() {
            log::error!("the message bus dropped {lost} deliveries for {node}: it was full");
            let Some(key) = NodeKey::from_wire(&node) else { continue };
            let message = if lost == 1 {
                "A message to this agent was dropped before it arrived: Velm's message bus was \
                 full. Nothing was delivered for it — ask again if it mattered."
                    .to_owned()
            } else {
                format!(
                    "{lost} messages to this agent were dropped before they arrived: Velm's \
                     message bus was full. Nothing was delivered for them — ask again if they \
                     mattered."
                )
            };
            self.record(&key, now, &TranscriptEvent::Error { message });
        }
        for delivery in self.bus.drain() {
            let Some(key) = NodeKey::from_wire(&delivery.node) else {
                continue;
            };
            // A message that arrived becomes the receiving agent's next prompt, carrying the
            // hop count the bus handed us. `MessageSent` is the sender's own copy and must
            // not prompt anybody.
            let mut refused = None;
            if let TranscriptEvent::Message { from, text } = &delivery.event
                && let Some(live) = self.sessions.get_mut(&key)
            {
                live.hops = delivery.hops;
                let prompt = format!("{} says: {text}", from.name);
                if let Err(error) = live.session.prompt(prompt) {
                    refused = Some(error.to_string());
                }
            }
            if let Some(message) = refused {
                self.record(&key, now, &TranscriptEvent::Error { message });
            }
            self.record(&key, now, &delivery.event);
        }
    }

    /// Serve everything an agent's own process asked for.
    ///
    /// Bounded per frame by the channel being empty rather than by a budget: a verb is a
    /// round trip an agent is blocked on, so the queue is as long as the number of agents,
    /// not as long as their output.
    fn drain_jobs(&mut self, now: Timestamp, now_ms: u64) {
        loop {
            let job = match self.inbox.try_recv() {
                Ok(job) => job,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            self.serve(job, now, now_ms);
        }
    }

    /// One verb. Everything that needs only files, the blob store or a transcript is done
    /// here; `spawn` and `configure` are deferred to the frame loop.
    fn serve(&mut self, job: Job, now: Timestamp, now_ms: u64) {
        let Job { agent, call, reply } = job;
        let Some(key) = NodeKey::from_wire(&agent) else {
            let _ = reply.send(Err(AgentError::Refused(format!(
                "\"{agent}\" is not an agent node on any open board"
            ))));
            return;
        };

        let answer = match call {
            Call::Send { to, text } => self.serve_send(&key, &to, &text, now_ms),
            Call::NoteRead { path } => self.serve_note_read(&key, &path),
            Call::NoteWrite { path, text, append } => {
                self.serve_note_write(&key, &path, &text, append)
            }
            Call::NoteList => self.serve_note_list(&key),
            Call::Image { bytes, caption } => self.serve_image(&key, &bytes, caption, now),
            Call::Options { prompt, choices } => {
                self.record(
                    &key,
                    now,
                    &TranscriptEvent::Options { prompt, choices, chosen: None },
                );
                Ok(Reply::Done)
            }
            // The three that need the document. Queued, never served here — see the module
            // header on undo groups.
            Call::Spawn(request) => {
                return self.defer(
                    DocumentWork::Spawn { parent: key, request: *request },
                    reply,
                );
            }
            Call::ReadConfig { node } => {
                let Some(node) = NodeKey::from_wire(&node) else {
                    let _ = reply.send(Err(AgentError::Refused(format!(
                        "\"{node}\" is not an agent node on any open board"
                    ))));
                    return;
                };
                return self.defer(DocumentWork::ReadConfig { node }, reply);
            }
            Call::WriteConfig { node, model } => {
                let Some(node) = NodeKey::from_wire(&node) else {
                    let _ = reply.send(Err(AgentError::Refused(format!(
                        "\"{node}\" is not an agent node on any open board"
                    ))));
                    return;
                };
                return self.defer(DocumentWork::WriteConfig { node, model }, reply);
            }
        };
        let _ = reply.send(answer);
    }

    /// Park a job for the frame loop.
    fn defer(&mut self, work: DocumentWork, reply: Sender<vellum_agent::Result<Reply>>) {
        let reply = Answering(reply);
        if self.deferred.len() >= MAX_DEFERRED {
            reply.refuse(format!(
                "Velm already has {MAX_DEFERRED} agent requests waiting — let it catch up"
            ));
            return;
        }
        self.deferred.push_back(Pending { work, reply });
    }

    /// Take the jobs that need the document.
    ///
    /// ⚠ **The caller must not call this while `ActiveState::busy_with_a_group()` is true.**
    /// These are applied inside `Editor::edit`, which opens an undo group, and a group
    /// opened while the caret or the eraser holds one fails — and then every later grouped
    /// operation on the board fails too, for the rest of the session. `apply_link_fetches`
    /// carries the same rule and the same reasoning: waiting costs a frame, committing costs
    /// the user's edit.
    pub fn take_document_jobs(&mut self) -> Vec<Pending> {
        self.deferred.drain(..).collect()
    }

    fn serve_send(
        &mut self,
        from: &NodeKey,
        to: &str,
        text: &str,
        now_ms: u64,
    ) -> vellum_agent::Result<Reply> {
        self.send(from, to, text, now_ms)?;
        Ok(Reply::Done)
    }

    /// Send one agent's message to another along a connector the user drew.
    ///
    /// The **same** function the `send` verb reaches, exposed so a `--demo` fixture can drive
    /// a message across a real board without a socket and a child process — one level below
    /// the wire and above everything the feature actually is: the label resolution, the
    /// connector rule, the direction, the hop bound and both transcripts.
    pub fn send(
        &mut self,
        from: &NodeKey,
        to: &str,
        text: &str,
        now_ms: u64,
    ) -> vellum_agent::Result<()> {
        // A label rather than an id is what an agent will type, and an ambiguous one is
        // refused rather than guessed — `Topology::resolve`'s rule.
        let target = self.bus.resolve(to).into_result(to)?;
        let hops = self.sessions.get(from).map_or(0, |live| live.hops);
        let message = Message::new(from.wire(), target, text).in_reply_at(hops);
        // The bus writes both transcripts as deliveries, including its own refusals, so
        // nothing here writes one as well — `Bus::send` says a caller that surfaces the
        // `Err` must not, or every refusal appears twice.
        self.bus.send(message, now_ms)
    }

    /// What a node should do when the turn a schedule started finishes.
    ///
    /// Recorded at fire time and applied at `TurnEnded`, because *"report what it found"* and
    /// *"hand the result to the next agent"* are both statements about a result that does not
    /// exist yet when the turn starts.
    pub fn expect_completion(&mut self, key: &NodeKey, completion: vellum_agent::Completion) {
        self.completions.insert(key.clone(), completion);
    }

    /// Everything a scheduled run asked to be reported, since the last call.
    pub fn take_reports(&mut self) -> Vec<(NodeKey, String)> {
        std::mem::take(&mut self.reports)
    }

    /// Apply the completion action for a turn that has just ended.
    fn complete(&mut self, key: &NodeKey, now: Timestamp, now_ms: u64) {
        let Some(completion) = self.completions.remove(key) else { return };
        // The turn's own last words, which is what both actions are about. Taken from the
        // ring rather than accumulated separately: the ring is already the record.
        let said = self
            .nodes
            .get(key)
            .and_then(|state| {
                state.events.iter().rev().find_map(|event| match event.as_ref() {
                    TranscriptEvent::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "the scheduled run finished with nothing to say".to_owned());

        match completion {
            vellum_agent::Completion::Report => self.reports.push((key.clone(), said)),
            vellum_agent::Completion::HandOff { agent } => {
                if let Err(error) = self.send(key, &agent, &said, now_ms) {
                    // Into this node's own transcript: the hand-off was its instruction, and
                    // an agent that was never told the message did not arrive cannot adapt.
                    let message = format!("the scheduled hand-off to {agent} failed: {error}");
                    self.record(key, now, &TranscriptEvent::Error { message });
                }
            }
            vellum_agent::Completion::Nothing => {}
        }
    }

    fn serve_note_read(&self, key: &NodeKey, path: &str) -> vellum_agent::Result<Reply> {
        let store = self.store_for(key)?;
        let file = store.resolve_stored(path)?;
        let who = key.wire();
        if !store.may_read_path(&file, Requester::Agent(&who)) {
            return Err(AgentError::Refused(format!(
                "{path} is private to another agent; {} may not read it",
                key.item
            )));
        }
        let text = std::fs::read_to_string(&file)
            .map_err(|error| AgentError::file(file.display().to_string(), &error))?;
        Ok(Reply::Text(text))
    }

    fn serve_note_write(
        &self,
        key: &NodeKey,
        path: &str,
        text: &str,
        append: bool,
    ) -> vellum_agent::Result<Reply> {
        let store = self.store_for(key)?;
        let file = store.resolve_stored(path)?;
        let who = key.wire();
        if !store.may_read_path(&file, Requester::Agent(&who)) {
            return Err(AgentError::Refused(format!(
                "{path} is private to another agent; {} may not write it",
                key.item
            )));
        }
        // Read-modify-write for an append, done here rather than by the agent, which is the
        // whole reason `append` is on the wire: two agents appending to one note through
        // read-then-write would each lose the other's line.
        let whole = if append {
            let existing = std::fs::read_to_string(&file).unwrap_or_default();
            if existing.is_empty() || existing.ends_with('\n') {
                format!("{existing}{text}")
            } else {
                format!("{existing}\n{text}")
            }
        } else {
            text.to_owned()
        };
        // Atomic: write a temporary and rename, so an agent reading mid-write never sees
        // half a file (§8).
        vellum_agent::notes::write_atomically(&file, &whole)?;
        Ok(Reply::Done)
    }

    /// Every note this agent may see: the board's shared notes, plus its own private ones.
    fn serve_note_list(&self, key: &NodeKey) -> vellum_agent::Result<Reply> {
        let store = self.store_for(key)?;
        let mut entries = Vec::new();
        collect_notes(store, store.root(), &NoteScope::Shared, &mut entries);
        let mine = NoteScope::Private { agent: key.wire() };
        collect_notes(store, &store.dir_for(&mine), &mine, &mut entries);
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Reply::Notes(entries))
    }

    fn serve_image(
        &mut self,
        key: &NodeKey,
        bytes: &[u8],
        caption: Option<String>,
        now: Timestamp,
    ) -> vellum_agent::Result<Reply> {
        // The **existing** blob store, addressed exactly like a pasted screenshot: so a
        // picture posted twice costs one copy, and the 268MB residency budget governs it for
        // free (§4).
        let hash = self
            .blobs
            .put(bytes)
            .map_err(|error| AgentError::Refused(format!("that picture would not store: {error}")))?;
        let event = TranscriptEvent::Image { blob: hash.to_hex().to_string(), caption };
        self.record(key, now, &event);
        Ok(Reply::Done)
    }

    fn store_for(&self, key: &NodeKey) -> vellum_agent::Result<&NoteStore> {
        self.stores.get(&key.board).ok_or_else(|| {
            AgentError::Refused("that agent's board is not open, so its notes are not reachable".into())
        })
    }

    // ----- the transcript ----------------------------------------------------------

    /// Append one event to the node's transcript **and** to the ring the painter reads.
    ///
    /// One function, so the two cannot come to disagree — a node that drew an event the file
    /// never got would look right and replay wrong.
    pub fn record(&mut self, key: &NodeKey, at: Timestamp, event: &TranscriptEvent) {
        // **Seed from disk first, then append.** Without this the ring holds only what this
        // process recorded, and the *next* `ensure_loaded` reads the file — which by then
        // contains those same events — and pushes them in again. The transcript silently
        // doubles the first time a node is drawn after it has spoken.
        //
        // Measured, not reasoned: `--demo agent-transcript` reported "clean showed 6
        // (expected 3), raw showed 12 (expected 6)". Exactly twice, which is the signature.
        self.ensure_loaded(key);

        let Self { sidecar, nodes, .. } = self;
        let state = nodes.entry(key.clone()).or_default();

        if state.appender.is_none() {
            match sidecar.appender(&key.board, &key.item) {
                Ok(appender) => state.appender = Some(appender),
                Err(error) => log::warn!("agents: opening a transcript failed ({error})"),
            }
        }
        let written = match state.appender.as_mut() {
            Some(appender) => appender.write(at, event),
            None => sidecar.append(&key.board, &key.item, at, event),
        };
        if let Err(error) = written {
            log::warn!("agents: appending to a transcript failed ({error})");
        }

        if let TranscriptEvent::Error { message } = event {
            state.resting = message.clone();
        }
        state.events.push_back(Arc::new(event.clone()));
        while state.events.len() > TAIL_EVENTS {
            state.events.pop_front();
            state.truncated = true;
        }
    }

    /// Seed a node's ring from disk, once.
    ///
    /// Called the first time a node becomes visible. A file read per node per *session*,
    /// never per frame: re-reading the JSONL to draw would be a disk read per agent per
    /// frame on a board of twenty.
    fn ensure_loaded(&mut self, key: &NodeKey) {
        let Self { sidecar, nodes, .. } = self;
        let state = nodes.entry(key.clone()).or_default();
        if state.loaded {
            return;
        }
        state.loaded = true;
        match sidecar.tail(&key.board, &key.item, TAIL_EVENTS) {
            Ok(tail) => {
                state.truncated = !tail.complete;
                for record in tail.records {
                    if let TranscriptEvent::Error { message } = &record.event {
                        state.resting = message.clone();
                    }
                    state.events.push_back(Arc::new(record.event));
                }
            }
            Err(error) => log::warn!("agents: reading a transcript failed ({error})"),
        }
    }

    /// What the user has typed into a node's prompt row and not sent.
    pub fn draft(&self, key: &NodeKey) -> &str {
        self.nodes.get(key).map_or("", |state| state.draft.as_str())
    }

    pub fn set_draft(&mut self, key: &NodeKey, text: impl Into<String>) {
        self.nodes.entry(key.clone()).or_default().draft = text.into();
    }

    /// Take the draft, leaving the row empty — what sending a prompt does.
    pub fn take_draft(&mut self, key: &NodeKey) -> String {
        self.nodes
            .get_mut(key)
            .map_or_else(String::new, |state| std::mem::take(&mut state.draft))
    }

    // ----- notes -------------------------------------------------------------------

    /// A note node's file, as last read.
    ///
    /// The painter is handed this through `AgentViews::set_note`, once per frame per
    /// **visible** note — a frame may not read a file, and a board of forty notes must not
    /// read forty of them to draw the two on screen.
    pub fn note_text(&self, key: &NodeKey) -> Option<&str> {
        self.note_text.get(key).map(String::as_str)
    }

    pub fn set_note_text(&mut self, key: &NodeKey, text: String) {
        self.note_text.insert(key.clone(), text);
    }

    /// The note model the last poll left, or `None` for one this process has not read.
    ///
    /// The caller seeds a fresh model from the document token — which holds the *path* and
    /// the *scope*, the two things that are genuinely board content — and then overlays this,
    /// which holds the stamp and the links. See the field for why the halves live apart.
    pub fn note_model(&self, key: &NodeKey) -> Option<&vellum_agent::NoteModel> {
        self.note_models.get(key)
    }

    pub fn set_note_model(&mut self, key: &NodeKey, model: vellum_agent::NoteModel) {
        self.note_models.insert(key.clone(), model);
    }

    /// Record the user's answer to an agent's question, in the ring the painter reads.
    ///
    /// Returns whether an unanswered question was found — `false` for a card that has already
    /// been picked, which is what stops one gesture sending two answers.
    ///
    /// ⚠ **In memory only, and the transcript on disk is not rewritten.** A sidecar is
    /// append-only JSONL (`vellum_agent::sidecar`), so recording the choice durably would mean
    /// either rewriting a file this layer only ever appends to or writing a second event that
    /// the ring would then draw as a *second* question. The consequence is stated rather than
    /// hidden: reopening a board shows the question unanswered again. That is honest — the
    /// session it was an answer to is gone, so asking again is the correct offer.
    pub fn choose_option(&mut self, key: &NodeKey, choice: &str) -> bool {
        let Some(state) = self.nodes.get_mut(key) else { return false };
        // Newest first: a long-running agent can ask more than one question, and the one on
        // screen being answered is the last one it asked.
        for event in state.events.iter_mut().rev() {
            let TranscriptEvent::Options { prompt, choices, chosen } = event.as_ref() else {
                continue;
            };
            if chosen.is_some() || !choices.iter().any(|option| option.id == choice) {
                continue;
            }
            // A fresh `Arc`, never `Arc::get_mut`: the painter is holding clones of these and
            // mutating one in place would change what a frame already decided to draw.
            *event = Arc::new(TranscriptEvent::Options {
                prompt: prompt.clone(),
                choices: choices.clone(),
                chosen: Some(choice.to_owned()),
            });
            return true;
        }
        false
    }

    /// Whether this note is due a freshness check, marking it checked if so.
    ///
    /// The rate limiter for §8's poll, kept here rather than at the call site because the
    /// call site is a loop over what is on screen and a timer per node is exactly the sort of
    /// bookkeeping that ends up per frame by accident.
    pub fn note_due(&mut self, key: &NodeKey, now: Timestamp) -> bool {
        let last = self.note_checked.get(key).copied().unwrap_or(0);
        if now.saturating_sub(last) < NOTE_POLL_SECONDS {
            return false;
        }
        self.note_checked.insert(key.clone(), now);
        true
    }

    // ----- views -------------------------------------------------------------------

    /// Fill in what the painter is told about this board's agents.
    ///
    /// **Viewport culling is the rule**: only nodes inside `visible` get a view, so an
    /// off-screen agent is not shaped, not tessellated and not uploaded while its process
    /// keeps running. That is the mechanic that makes dozens of agents affordable.
    ///
    /// The display mode is resolved here and the events are filtered and bounded here,
    /// because `agent_view.rs` says the expensive decision belongs in one place — and
    /// `TranscriptEvent::visible_in_clean_mode` is deliberately the only definition of what
    /// Clean mode shows, so it is *called* rather than re-derived.
    ///
    /// # It fills the caller's set rather than returning a new one
    ///
    /// `AgentViews::clear` empties the frame's answers **and keeps the allocations**, which is
    /// what its own doc comment has always promised and what returning a fresh
    /// `AgentViews::new()` here quietly made false: three `HashMap`s were allocated and
    /// dropped on every frame a board had an agent on it. Taking `&mut` is what makes the
    /// promise true, and it is the same reason `DrawList` is owned and reused.
    pub fn rebuild_views(
        &mut self,
        views: &mut crate::agent_view::AgentViews,
        board: &BoardKey,
        projection: &crate::project::Projection,
        visible: vellum_scene::WorldRect,
        fallback: DisplayMode,
        now_ms: u64,
    ) {
        views.clear();

        // What is on screen, through the same R-tree the painter culls with.
        let visible_ids: Vec<vellum_scene::ItemId> =
            projection.scene().query_rect(visible).map(|item| item.id).collect();

        for scene in &visible_ids {
            let Some(projected) = projection.get(*scene) else { continue };
            let vellum_doc::ItemKind::Agent { model, .. } = &projected.item.kind else {
                continue;
            };
            let key = NodeKey::new(board, projected.doc_id.to_string());
            self.ensure_loaded(&key);

            let config = crate::agent::decode(model);
            let mode = crate::agent::display_mode(&config, fallback);
            let (status, detail) = match self.sessions.get(&key) {
                Some(live) => (live.session.status(), live.session.detail().to_owned()),
                None => {
                    let resting = self
                        .nodes
                        .get(&key)
                        .map_or_else(String::new, |state| state.resting.clone());
                    let status = if resting.is_empty() { Status::Idle } else { Status::Error };
                    (status, resting)
                }
            };

            let state = self.nodes.entry(key.clone()).or_default();
            // Filtered for the mode, then bounded to the tail — in that order, or a node in
            // Clean mode whose last forty events were all tool calls would draw nothing.
            //
            // **The events are shared, not copied.** This runs once a frame for every visible
            // node, and a `Text` event carries the agent's whole answer — so cloning forty of
            // them per node per frame was a deep string copy of most of a conversation, sixty
            // times a second, on a board that is not doing anything. See `NodeState::events`.
            let kept: Vec<&Arc<TranscriptEvent>> = state
                .events
                .iter()
                .filter(|event| mode == DisplayMode::Raw || event.visible_in_clean_mode())
                .collect();
            let truncated = state.truncated || kept.len() > VIEW_EVENTS;
            // The **last** `VIEW_EVENTS`, because what is on screen is the end of a
            // conversation rather than its beginning.
            let from = kept.len().saturating_sub(VIEW_EVENTS);
            let events: Vec<Arc<TranscriptEvent>> =
                kept[from..].iter().map(|event| Arc::clone(event)).collect();

            views.insert(
                *scene,
                crate::agent_view::AgentView {
                    status,
                    detail,
                    subtitle: crate::agent::subtitle(&config),
                    mode,
                    events,
                    truncated,
                    draft: state.draft.clone(),
                    // The *stored* draft, always. The live buffer belongs to the keyboard,
                    // which this module deliberately knows nothing about — the app puts it
                    // over the top afterwards. See `AgentView::draft`.
                    caret: None,
                },
            );
        }

        // Pulses, and the early-out that makes an idle board cost one boolean. A board with
        // fifty connectors on screen and nothing happening asks this once rather than fifty
        // times.
        if self.bus.any_in_flight(now_ms) {
            for scene in &visible_ids {
                let Some(projected) = projection.get(*scene) else { continue };
                let vellum_doc::ItemKind::Connector { start, end, .. } = &projected.item.kind
                else {
                    continue;
                };
                let (Some(a), Some(b)) = (start.target, end.target) else { continue };
                let a = NodeKey::new(board, a.to_string()).wire();
                let b = NodeKey::new(board, b.to_string()).wire();
                if let Some(pulse) = self.bus.pulse(&a, &b, now_ms) {
                    views.add_pulse(*scene, pulse);
                }
            }
        }
    }

    // ----- scheduling ---------------------------------------------------------------

    /// Arm the scheduler with everything **one board** has, leaving every other board's
    /// schedules exactly as they were.
    ///
    /// ⚠ **Per board, and it was a defect that it was not.** The scheduler holds one queue,
    /// and this used to replace the whole of it with the walk of whichever board was in
    /// front — so switching tabs disarmed every schedule on the board you left. An agent set
    /// to run at six in the evening simply did not, and nothing on screen said so.
    /// [`AgentRuntime::set_wiring`] was already careful about exactly this and its neighbour
    /// was not.
    ///
    /// One thread with a sorted queue that **wakes for the next fire time**, not a poll — and
    /// it does not exist at all while nothing is scheduled *anywhere*: the union going empty
    /// stops it outright. Re-armed from the document, so a run that writes `last_run`
    /// re-enters here on the next epoch and the loop closes itself.
    pub fn set_schedules(
        &mut self,
        board: &BoardKey,
        entries: Vec<(NodeKey, Schedule)>,
        now: Timestamp,
    ) {
        let armed: Vec<(Timestamp, NodeKey)> = entries
            .into_iter()
            .filter_map(|(key, schedule)| schedule.next_fire(now).map(|at| (at, key)))
            .collect();
        if armed.is_empty() {
            self.schedules.remove(board);
        } else {
            self.schedules.insert(board.clone(), armed);
        }
        self.rearm();
    }

    /// Rebuild the one queue the scheduler thread holds from every board's entries.
    ///
    /// The union rather than one thread per board: a thread that sleeps until the next fire
    /// time costs nothing while it sleeps, and one of them can carry every board's times just
    /// as well as five can — while five is five sets of OS handles for a feature whose whole
    /// argument is that it costs nothing when unused.
    fn rearm(&mut self) {
        let mut queue: Vec<(Timestamp, NodeKey)> =
            self.schedules.values().flatten().cloned().collect();
        queue.sort_by_key(|(at, _)| *at);

        if queue.is_empty() {
            // Take what fired before dropping the thread that reported it, or a schedule that
            // came due in the same frame the list emptied would be lost.
            self.collect_due();
            self.scheduler = None;
            return;
        }
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler.arm(queue);
            return;
        }
        self.scheduler = Some(Scheduler::start(queue));
    }

    /// Overlay what this process remembers about a schedule's history onto a model decoded
    /// from the document.
    ///
    /// Called everywhere a schedule is *used* — arming it and checking its trigger — so there
    /// is one answer to "when did this last run" rather than a document that has one and a
    /// runtime that has another. See [`AgentRuntime::schedule_runs`] for why the answer is not
    /// in the document at all.
    pub fn apply_schedule_history(&self, key: &NodeKey, schedule: &mut Schedule) {
        let Some((last_run, last_failed)) = self.schedule_runs.get(key) else { return };
        schedule.last_run = *last_run;
        schedule.last_failed = *last_failed;
    }

    /// Record that a schedule fired, whether or not its trigger held.
    ///
    /// **Whether or not**, deliberately, and that is what the document write got right and is
    /// kept: a declined fire that did not move `last_run` stayed past due, so every later edit
    /// to the board re-armed it, fired it again and logged another skipped line.
    pub fn note_schedule_run(&mut self, key: &NodeKey, at: Timestamp, failed: bool) {
        self.schedule_runs.insert(key.clone(), (Some(at), failed));
    }

    /// Put a fired schedule back, because the board it belongs to is not the one in front.
    ///
    /// A run needs the *document* — the trigger, the model, the `last_run` write and the
    /// completion action all read or write the board — and `crate::actions` only ever holds
    /// the board on screen. So a key whose board is parked waits here rather than being
    /// dropped, which is what it used to be: `take_due` handed it over, `agent_doc` could not
    /// resolve it, and the arm that could not resolve it simply moved on.
    ///
    /// A board that is *closed* rather than parked never comes front again, so its waiting
    /// keys are dropped by [`AgentRuntime::forget_board`] rather than left here for the life
    /// of the process — `due` is one of the six things [`AgentRuntime::dormant`] tests, so a
    /// key nothing will ever collect keeps the whole layer awake.
    ///
    /// De-duplicated, because a board can stay parked across many frames and each one would
    /// otherwise put the same key back again.
    pub fn defer_due(&mut self, key: NodeKey) {
        if self.due.contains(&key) {
            return;
        }
        self.due.push(key);
    }

    /// A board's tab was closed: stop its agents and forget everything derived from it.
    ///
    /// Closing is not parking. A parked board keeps its sessions and its schedules — that is
    /// what the tab strip is for — but a closed one has no way left to be looked at, so a
    /// process still running under it could never be stopped and a schedule still armed under
    /// it would fire into a board nobody can see. Its **transcripts survive**: they are the
    /// record of what happened and reopening the board shows them again.
    pub fn forget_board(&mut self, board: &BoardKey) {
        let running: Vec<NodeKey> = self
            .sessions
            .keys()
            .filter(|key| &key.board == board)
            .cloned()
            .collect();
        for key in running {
            self.release(&key);
        }
        self.due.retain(|key| &key.board != board);
        self.schedule_runs.retain(|key, _| &key.board != board);
        self.completions.retain(|key, _| &key.board != board);
        self.reports.retain(|(key, _)| &key.board != board);
        self.schedules.remove(board);
        self.wiring.remove(board);
        self.stores.remove(board);
        if let Ok(mut table) = self.roles.write() {
            let prefix = format!("{board}:");
            table.retain(|id, _| !id.starts_with(&prefix));
        }
        self.rebuild_topology();
        self.rearm();
    }

    /// Move anything the scheduler has fired into [`AgentRuntime::due`].
    fn collect_due(&mut self) {
        // Collected into a local first: the receiver is inside `self.scheduler` and the
        // queue it feeds is `self.due`, so reading straight into the second would hold a
        // borrow of the first across it.
        let mut fired: Vec<NodeKey> = Vec::new();
        if let Some(scheduler) = self.scheduler.as_ref() {
            while let Ok(key) = scheduler.fires.try_recv() {
                fired.push(key);
            }
        }
        self.due.append(&mut fired);
    }

    /// Take the schedules that have come due, for the caller to check their triggers and run
    /// their turns.
    ///
    /// The trigger is checked by the caller rather than here because two of the four
    /// ([`vellum_agent::Trigger::FilesChanged`], `NoteChanged`) are questions about the
    /// filesystem the *board* knows the shape of, and the completion action needs the
    /// document.
    pub fn take_due(&mut self) -> Vec<NodeKey> {
        std::mem::take(&mut self.due)
    }

    // ----- away-mode ---------------------------------------------------------------

    /// The window gained or lost focus.
    ///
    /// The app knows about focus and `vellum_agent::summary` takes "since" as a parameter, so
    /// this is the whole of the app's half: remember when they left, and answer how long they
    /// were gone.
    ///
    /// Returns `Some(since)` when the user has come back from being away long enough to be
    /// worth a digest — [`AWAY_THRESHOLD`] — and `None` otherwise, so the caller has one
    /// question to ask rather than two facts to compare.
    pub fn focus_changed(&mut self, focused: bool, now: Timestamp) -> Option<Timestamp> {
        if !focused {
            // Only the *first* loss counts. Re-recording it would restart the clock every
            // time the window server reported focus twice, which it does.
            self.away_since.get_or_insert(now);
            return None;
        }
        let since = self.away_since.take()?;
        (now.saturating_sub(since) >= AWAY_THRESHOLD).then_some(since)
    }

    /// Every node that has a transcript on this board, for the digest.
    pub fn nodes_on(&self, board: &BoardKey) -> Vec<NodeKey> {
        self.nodes
            .keys()
            .filter(|key| &key.board == board)
            .cloned()
            .collect()
    }

    // ----- shutdown -----------------------------------------------------------------

    /// Stop everything, in the order that cannot deadlock.
    ///
    /// See the module header: the flag first so nothing new can park, then every parked job
    /// answered, and only then the server joined. Idempotent.
    pub fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);

        // Answer what is already in flight, both queues, before the join. A refusal is a
        // sentence an agent can act on; a dropped connection is not.
        while let Ok(job) = self.inbox.try_recv() {
            let _ = job.reply.send(Err(AgentError::Refused("Velm is closing".into())));
        }
        for pending in self.deferred.drain(..) {
            pending.reply.refuse("Velm is closing");
        }

        if let Some(mut server) = self.ipc.take() {
            server.stop();
        }
        self.scheduler = None;
        for live in self.sessions.values_mut() {
            let _ = live.session.shutdown();
        }
        self.sessions.clear();
        // Releases every held-open transcript handle. The files are complete: every append
        // is one unbuffered `write_all`, which is exactly why `sidecar::Appender` is not a
        // `BufWriter`.
        self.nodes.clear();
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Turn a picture's placeholder into the hash its bytes were stored under — or, when they
/// would not store, into an error.
///
/// Free and pure so the rule can be tested without a `Session`, which owns a process and a
/// thread and cannot be built in a unit test. The ordering that makes it correct is
/// [`AgentRuntime::drain_sessions`]'s; this is only what to do once both halves are in hand.
///
/// Three cases, and the third is the one that used to be silently wrong:
///
/// - **Stored.** The placeholder becomes the hash and the transcript holds a picture.
/// - **Would not store.** The event becomes an [`TranscriptEvent::Error`] naming what
///   happened. Recording the `Image` anyway wrote a placeholder into the JSONL that no later
///   frame could ever resolve, so the node showed a broken picture for the life of the board
///   rather than a sentence saying the picture was lost.
/// - **Neither.** Left exactly as it is — a hash from `serve_image` is already final, and a
///   placeholder that has been neither stored nor refused is not this function's to judge.
fn resolve_blob(
    event: &mut TranscriptEvent,
    names: &HashMap<String, String>,
    failed: &HashSet<String>,
) {
    let TranscriptEvent::Image { blob, caption } = event else { return };
    // Resolved to an owned value first: the placeholder is read out of the same string that
    // is about to be written over, and taking a copy is what keeps that a plain assignment.
    if let Some(hash) = names.get(blob.as_str()).cloned() {
        *blob = hash;
        return;
    }
    if !failed.contains(blob.as_str()) {
        return;
    }
    let what = match caption.as_deref() {
        Some(caption) if !caption.trim().is_empty() => format!(" ({caption})"),
        _ => String::new(),
    };
    *event = TranscriptEvent::Error {
        message: format!("a picture this agent posted{what} could not be stored"),
    };
}

/// Every `.md` in one directory, as note entries. Not recursive: a private note is one
/// directory down and is listed by its own call, and the store's layout *is* the scope.
fn collect_notes(store: &NoteStore, dir: &Path, scope: &NoteScope, out: &mut Vec<NoteEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("md") {
            continue;
        }
        let title = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| heading_of(&text))
            .unwrap_or_default();
        out.push(NoteEntry { path: store.stored_path(&path), scope: scope.clone(), title });
    }
}

/// How many directory entries a "did anything change" walk will look at.
///
/// Bounded because the answer is wanted at a schedule's fire time and a working directory can
/// be a monorepo. Running when nothing changed is a wasted turn; hanging the frame loop
/// walking a million files is a broken application, so the bound errs towards the wasted turn.
const CHANGE_BUDGET: usize = 4_000;

/// The newest modification time anywhere under `root`, or `None` if nothing could be read.
///
/// What [`vellum_agent::Trigger::FilesChanged`] is asked. Deliberately **not** a filesystem
/// watcher: a watcher is a thread and a set of OS handles that would exist for every
/// scheduled agent whether or not it ever fires, which is exactly the idle cost this layer
/// refuses. A schedule fires at most once an interval, and a bounded walk at that moment is
/// paid only by the boards that asked for it.
///
/// `.git`, `target` and `node_modules` are skipped: they change constantly and for reasons
/// that are not the user's edits, so counting them would make the trigger always true — which
/// is the same as not having one.
pub fn newest_mtime(root: &Path) -> Option<u64> {
    fn stamp(entry: &std::fs::Metadata) -> Option<u64> {
        entry
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs())
    }

    let mut newest: Option<u64> = None;
    let mut budget = CHANGE_BUDGET;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if budget == 0 {
                return newest;
            }
            budget -= 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), ".git" | "target" | "node_modules") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else { continue };
            if metadata.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if let Some(at) = stamp(&metadata) {
                newest = Some(newest.map_or(at, |seen: u64| seen.max(at)));
            }
        }
    }
    newest
}

/// A note's first `#` heading, so a listing reads as a set of documents rather than as a set
/// of filenames.
fn heading_of(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        let heading = line.trim_start().strip_prefix('#')?;
        let text = heading.trim_start_matches('#').trim();
        (!text.is_empty()).then(|| text.chars().take(80).collect())
    })
}

// ---------------------------------------------------------------------------------------
// The IPC handler
// ---------------------------------------------------------------------------------------

/// `vellum-agent`'s seam, implemented as a channel pair drained once per frame.
///
/// The trait is called from the server's own thread, so every method here does the same
/// thing: park a job, wait for the frame loop to answer it, hand the answer back. Nothing in
/// this type touches a board — that is the entire reason the seam exists.
///
/// [`IpcHandler::role_of`] is the exception and is answered from a snapshot: it runs on every
/// request before the verb does, and a round trip for it would double the latency of every
/// call to answer a question that only changes when the user edits a node.
struct Handler {
    /// Behind a `Mutex` because the trait is `Sync` and a `Sender` need not be on every
    /// toolchain this has to build on. The lock is held for the length of one `send`.
    post: Mutex<Sender<Job>>,
    roles: Arc<RwLock<HashMap<String, RoleKind>>>,
    stopping: Arc<AtomicBool>,
}

impl Handler {
    fn call(&self, agent: &str, call: Call) -> vellum_agent::Result<Reply> {
        // **Before parking**, which is what makes the join in `IpcServer::stop` safe. See
        // the module header.
        if self.stopping.load(Ordering::SeqCst) {
            return Err(AgentError::Refused("Velm is closing".into()));
        }
        let (reply, answer) = channel();
        let job = Job { agent: agent.to_owned(), call, reply };
        {
            let post = self
                .post
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            post.send(job).map_err(|_| {
                AgentError::Refused("Velm is no longer serving agent requests".into())
            })?;
        }
        // `recv_timeout`, never `recv`: an unbounded wait here is the deadlock `ipc.rs`
        // warns about, and a bounded one turns it into a refusal the agent can read.
        match answer.recv_timeout(REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(AgentError::Refused(
                "Velm did not answer in time — it may be busy with an edit".into(),
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(AgentError::Refused("Velm stopped serving that request".into()))
            }
        }
    }
}

impl IpcHandler for Handler {
    fn role_of(&self, agent: &str) -> Option<RoleKind> {
        self.roles.read().ok()?.get(agent).copied()
    }

    fn send_message(&self, from: &str, to: &str, text: &str) -> vellum_agent::Result<()> {
        self.call(from, Call::Send { to: to.to_owned(), text: text.to_owned() })
            .map(|_| ())
    }

    fn note_read(&self, agent: &str, path: &str) -> vellum_agent::Result<String> {
        match self.call(agent, Call::NoteRead { path: path.to_owned() })? {
            Reply::Text(text) => Ok(text),
            other => Err(wrong_answer("note.read", &other)),
        }
    }

    fn note_write(
        &self,
        agent: &str,
        path: &str,
        text: &str,
        append: bool,
    ) -> vellum_agent::Result<()> {
        self.call(
            agent,
            Call::NoteWrite { path: path.to_owned(), text: text.to_owned(), append },
        )
        .map(|_| ())
    }

    fn note_list(&self, agent: &str) -> vellum_agent::Result<Vec<NoteEntry>> {
        match self.call(agent, Call::NoteList)? {
            Reply::Notes(notes) => Ok(notes),
            other => Err(wrong_answer("note.list", &other)),
        }
    }

    fn spawn(&self, parent: &str, request: &SpawnRequest) -> vellum_agent::Result<String> {
        match self.call(parent, Call::Spawn(Box::new(request.clone())))? {
            Reply::Spawned(agent) => Ok(agent),
            other => Err(wrong_answer("spawn", &other)),
        }
    }

    fn post_image(
        &self,
        agent: &str,
        bytes: &[u8],
        caption: Option<&str>,
    ) -> vellum_agent::Result<()> {
        self.call(
            agent,
            Call::Image { bytes: bytes.to_vec(), caption: caption.map(str::to_owned) },
        )
        .map(|_| ())
    }

    fn post_options(
        &self,
        agent: &str,
        prompt: &str,
        choices: &[Choice],
    ) -> vellum_agent::Result<()> {
        self.call(
            agent,
            Call::Options { prompt: prompt.to_owned(), choices: choices.to_vec() },
        )
        .map(|_| ())
    }

    fn read_config(&self, node: &str) -> vellum_agent::Result<AgentModel> {
        // The *caller* is the meta agent and `node` is what it is asking about; the server
        // has already checked the capability against the caller's role. The job is posted
        // under the node's id because that is the board the answer comes from.
        match self.call(node, Call::ReadConfig { node: node.to_owned() })? {
            Reply::Config(model) => Ok(*model),
            other => Err(wrong_answer("configure", &other)),
        }
    }

    fn write_config(&self, node: &str, model: AgentModel) -> vellum_agent::Result<()> {
        self.call(
            node,
            Call::WriteConfig { node: node.to_owned(), model: Box::new(model) },
        )
        .map(|_| ())
    }

    fn log_refusal(&self, agent: &str, verb: &str, reason: &str) {
        // Into the log rather than stderr, so it reaches the flight recorder's file with
        // everything else that happened around it — a refusal nobody records is a security
        // event nobody can investigate.
        log::warn!("agents: refused {verb} from {agent}: {reason}");
    }
}

/// A reply of the wrong shape is a bug in this file, not something an agent can act on — but
/// it must still be a sentence rather than a panic, because it would be raised on the
/// server's thread.
fn wrong_answer(verb: &str, reply: &Reply) -> AgentError {
    log::error!("agents: {verb} answered with {reply:?}, which is the wrong shape");
    AgentError::Refused(format!("{verb} answered with something Velm could not read"))
}

// ---------------------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------------------

/// The upper bound on one wait, so a system clock jump cannot strand a schedule.
///
/// **Not a poll.** A schedule an hour out is waited for once; one a week out re-checks the
/// clock hourly, which is a wake per hour for a thread that would otherwise be asleep
/// forever if the machine's clock moved backwards behind it.
const MAX_WAIT: u64 = 3_600;

/// One thread, a sorted queue, and a condition variable.
///
/// It **does not exist while nothing is scheduled** (`docs/07` §10): armed with an empty
/// list, [`AgentRuntime::set_schedules`] drops it outright, and the thread returns of its own
/// accord once its queue empties.
struct Scheduler {
    queue: Arc<Mutex<Vec<(Timestamp, NodeKey)>>>,
    wake: Arc<Condvar>,
    stopping: Arc<AtomicBool>,
    fires: Receiver<NodeKey>,
    /// A sender the runtime keeps and never sends on.
    ///
    /// `mpsc::Receiver` cannot hand one out, and the thread's own copy goes with the thread
    /// when it runs itself out — so a restart would otherwise need a new channel, and
    /// anything fired into the old one between the two would be lost. Keeping a spare means
    /// a restarted thread joins the *same* channel the runtime is already reading.
    spare: Sender<NodeKey>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Scheduler {
    fn start(queue: Vec<(Timestamp, NodeKey)>) -> Self {
        let (spare, fires) = channel();
        let mut scheduler = Self {
            queue: Arc::new(Mutex::new(queue)),
            wake: Arc::new(Condvar::new()),
            stopping: Arc::new(AtomicBool::new(false)),
            fires,
            spare,
            thread: None,
        };
        scheduler.restart();
        scheduler
    }

    /// Replace the queue and wake the thread, restarting it if it has run itself out.
    fn arm(&mut self, queue: Vec<(Timestamp, NodeKey)>) {
        {
            let mut held = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *held = queue;
            // **Notified while the lock is held.** The thread checks its flag and its queue
            // under the same lock before waiting, so a notify sent without it can land in the
            // gap between the check and the wait — and the next wake would then be up to
            // `MAX_WAIT` away, which for a re-arm is an hour of a schedule not firing.
            self.wake.notify_all();
        }

        // The thread returns when its queue empties, which is what "does not exist while
        // nothing is scheduled" means in practice — so re-arming has to be able to start a
        // new one rather than assume the old one is listening.
        let finished = self
            .thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished);
        if finished {
            if let Some(old) = self.thread.take() {
                let _ = old.join();
            }
            self.restart();
        }
    }

    /// Start a thread on the existing channel.
    fn restart(&mut self) {
        let shared = Arc::clone(&self.queue);
        let wake = Arc::clone(&self.wake);
        let stopping = Arc::clone(&self.stopping);
        let fired = self.spare.clone();
        self.thread = std::thread::Builder::new()
            .name("velm-schedule".into())
            .spawn(move || run_schedule(&shared, &wake, &stopping, &fired))
            .ok();
        if self.thread.is_none() {
            log::warn!("agents: the scheduler thread would not start; schedules will not fire");
        }
    }

    fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        {
            // Under the lock, for the reason `arm` gives — and here it is worse: a missed
            // notify would make the `join` below wait out the thread's whole timeout.
            let _held = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The scheduler's thread: wake for the next fire time, never on a poll.
fn run_schedule(
    queue: &Mutex<Vec<(Timestamp, NodeKey)>>,
    wake: &Condvar,
    stopping: &AtomicBool,
    fired: &Sender<NodeKey>,
) {
    let mut held = queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let now = unix_now();
        let mut due: Vec<NodeKey> = Vec::new();
        held.retain(|(at, key)| {
            if *at <= now {
                due.push(key.clone());
                false
            } else {
                true
            }
        });
        if !due.is_empty() {
            // The lock is not held across the send: the frame loop takes it too, and a
            // scheduler holding it while a full channel blocked would stall a frame.
            drop(held);
            for key in due {
                if fired.send(key).is_err() {
                    return;
                }
            }
            held = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        }
        if held.is_empty() {
            // Nothing left to wait for, so the thread stops existing. `Scheduler::arm`
            // starts a replacement when something is scheduled again.
            return;
        }
        let next = held.iter().map(|(at, _)| *at).min().unwrap_or(now);
        let wait = Duration::from_secs(next.saturating_sub(now).clamp(1, MAX_WAIT));
        let (guard, _timed_out) = wake
            .wait_timeout(held, wait)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held = guard;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own.
    ///
    /// ⚠ **Named from a counter, not the clock.** It used to be `pid`-`unix_now()`, and
    /// `unix_now` is *seconds* — so any two tests that started within the same second got the
    /// **same directory**, and the first one to finish ran `remove_dir_all` on the other's
    /// data. `cargo test` runs these in parallel, so that is the common case rather than the
    /// unlucky one.
    ///
    /// It presented as two unrelated failures — "the ring was not seeded from disk" and
    /// "releasing a session destroyed its history" — both of which look exactly like a bug in
    /// the sidecar. Neither was. The counter cannot collide, and it needs no clock, which
    /// this crate is trying to avoid reading anyway.
    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("velm-agent-runtime-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn open(dir: &Path) -> AgentRuntime {
        let blobs = BlobStore::open(dir.join("blobs")).expect("a scratch blob store");
        AgentRuntime::new(dir, blobs)
    }

    fn board() -> BoardKey {
        BoardKey::from_raw("00000000deadbeef")
    }

    /// The property the whole layer rests on. If this stops being true, every board in the
    /// application pays for a feature it is not using.
    #[test]
    fn a_runtime_with_no_agents_is_dormant_and_drains_to_nothing() {
        let dir = scratch();
        let mut runtime = open(&dir);
        assert!(runtime.dormant());
        runtime.drain(1_000);
        assert!(runtime.dormant(), "draining an idle runtime woke something up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wire id has to survive the round trip exactly, because it is what an agent's own
    /// process is handed in `VELM_AGENT_ID` and what every refusal names. A Loro `TreeID`
    /// carries an `@`, which is the character a naive separator would have collided with.
    #[test]
    fn a_node_key_round_trips_through_its_wire_form() {
        let key = NodeKey::new(&board(), "12@7654321");
        let wire = key.wire();
        assert_eq!(wire, "00000000deadbeef:12@7654321");
        assert_eq!(NodeKey::from_wire(&wire), Some(key));

        for refused in ["", ":", "board:", ":item", "no-separator"] {
            assert!(NodeKey::from_wire(refused).is_none(), "{refused} was accepted");
        }
    }

    /// An event has to reach the ring **and** the file, from one call, or a node draws a
    /// transcript that replays as something else after a restart.
    #[test]
    fn recording_an_event_reaches_both_the_ring_and_the_disk() {
        let dir = scratch();
        let mut runtime = open(&dir);
        let key = NodeKey::new(&board(), "1@1");

        runtime.record(&key, 100, &TranscriptEvent::Text { text: "hello".into() });
        runtime.record(&key, 101, &TranscriptEvent::Text { text: "again".into() });

        let tail = runtime
            .sidecar()
            .tail(&key.board, &key.item, 10)
            .expect("the transcript reads back");
        assert_eq!(tail.records.len(), 2, "the file did not get both events");
        assert_eq!(tail.records[0].at, 100);

        // And a second runtime over the same directory — the restart case — sees them.
        let mut reopened = open(&dir);
        reopened.ensure_loaded(&key);
        let state = reopened.nodes.get(&key).expect("the node was seeded");
        assert_eq!(state.events.len(), 2, "the ring was not seeded from disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ring is bounded, and it **says** it is bounded. A node that silently dropped the
    /// middle of a long run would look like an agent that never finished a sentence.
    #[test]
    fn the_ring_is_bounded_and_reports_that_it_dropped_history() {
        let dir = scratch();
        let mut runtime = open(&dir);
        let key = NodeKey::new(&board(), "2@1");
        for index in 0..TAIL_EVENTS + 5 {
            runtime.record(&key, 1, &TranscriptEvent::Text { text: format!("line {index}") });
        }
        let state = runtime.nodes.get(&key).expect("the node exists");
        assert_eq!(state.events.len(), TAIL_EVENTS);
        assert!(state.truncated, "the ring dropped history without saying so");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Away-mode's whole condition. Alt-tabbing to a browser and straight back is not being
    /// away, and a digest that appeared every time the window lost focus is a notification
    /// nobody would read.
    #[test]
    fn coming_back_raises_a_digest_only_after_a_real_absence() {
        let dir = scratch();
        let mut runtime = open(&dir);

        assert_eq!(runtime.focus_changed(false, 1_000), None, "leaving is not returning");
        assert_eq!(
            runtime.focus_changed(true, 1_000 + AWAY_THRESHOLD - 1),
            None,
            "a glance at another window raised a digest"
        );

        runtime.focus_changed(false, 2_000);
        // A second report of the same loss must not restart the clock — the window server
        // does report focus twice.
        runtime.focus_changed(false, 2_500);
        assert_eq!(
            runtime.focus_changed(true, 2_000 + AWAY_THRESHOLD),
            Some(2_000),
            "a real absence did not raise a digest, or it dated it from the wrong moment"
        );
        assert_eq!(runtime.focus_changed(true, 9_999), None, "focus without a loss");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scheduler must not exist while nothing is scheduled — `docs/07` §10, and the
    /// second half of the no-idle-cost rule.
    #[test]
    fn nothing_scheduled_means_no_scheduler_thread() {
        let dir = scratch();
        let mut runtime = open(&dir);
        assert!(runtime.scheduler.is_none());

        runtime.set_schedules(&board(), Vec::new(), 1_000);
        assert!(runtime.scheduler.is_none(), "an empty list started a thread");

        let key = NodeKey::new(&board(), "3@1");
        runtime.set_schedules(&board(), vec![(key, hourly())], 1_000);
        assert!(runtime.scheduler.is_some(), "an armed schedule started nothing");

        runtime.set_schedules(&board(), Vec::new(), 1_000);
        assert!(runtime.scheduler.is_none(), "disarming left the thread running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hourly() -> Schedule {
        Schedule {
            enabled: true,
            recurrence: vellum_agent::Recurrence::Interval { minutes: 60 },
            ..Schedule::default()
        }
    }

    /// ⚠ **Switching tabs used to disarm every schedule on the board you left.** The
    /// scheduler holds one queue and `set_schedules` replaced the whole of it with the front
    /// board's walk, so an agent set to run at six in the evening on a parked board simply
    /// did not — and nothing said so.
    ///
    /// A/B: with the per-board map removed, the second call below leaves one entry in the
    /// queue instead of two and the first board's key is gone.
    #[test]
    fn arming_one_board_leaves_another_boards_schedules_armed() {
        let dir = scratch();
        let mut runtime = open(&dir);
        let (first, second) = (board(), BoardKey::from_raw("00000000feedface"));
        let a = NodeKey::new(&first, "3@1");
        let b = NodeKey::new(&second, "4@1");

        runtime.set_schedules(&first, vec![(a.clone(), hourly())], 1_000);
        // The other tab comes front and derives *its* wiring, which is the whole gesture.
        runtime.set_schedules(&second, vec![(b.clone(), hourly())], 1_000);

        let armed: Vec<NodeKey> = runtime
            .schedules
            .values()
            .flatten()
            .map(|(_, key)| key.clone())
            .collect();
        assert!(armed.contains(&a), "the parked board's schedule was disarmed by a tab switch");
        assert!(armed.contains(&b));

        // And withdrawing one board's schedules leaves the other's alone.
        runtime.set_schedules(&second, Vec::new(), 1_000);
        assert!(runtime.schedules.contains_key(&first));
        assert!(runtime.scheduler.is_some(), "the surviving schedule lost its thread");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing a tab is not parking it: nothing is left that could stop an agent by hand, so
    /// its process, its schedule and anything it had waiting go with the board. Its
    /// transcript does **not** — that is the record of what happened.
    #[test]
    fn closing_a_board_takes_its_schedules_and_its_waiting_work_with_it() {
        let dir = scratch();
        let mut runtime = open(&dir);
        let (first, second) = (board(), BoardKey::from_raw("00000000feedface"));
        let a = NodeKey::new(&first, "3@1");
        let b = NodeKey::new(&second, "4@1");

        runtime.set_schedules(&first, vec![(a.clone(), hourly())], 1_000);
        runtime.set_schedules(&second, vec![(b.clone(), hourly())], 1_000);
        runtime.record(&a, 1, &TranscriptEvent::Text { text: "ran".into() });
        runtime.defer_due(a.clone());
        runtime.defer_due(a.clone());
        assert_eq!(runtime.due.len(), 1, "the same key was deferred twice");
        assert!(!runtime.dormant(), "a waiting schedule left the runtime dormant");

        runtime.forget_board(&first);
        assert!(runtime.due.is_empty(), "a closed board left work nothing will ever collect");
        assert!(!runtime.schedules.contains_key(&first));
        assert!(runtime.schedules.contains_key(&second), "closing one board disarmed another");
        assert!(
            runtime.sidecar().read_all(&first, &a.item).is_ok_and(|tail| !tail.records.is_empty()),
            "closing a board destroyed its transcript"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⚠ **A picture that would not store used to be written into the transcript as
    /// `pending:N`, permanently.** The JSONL is append-only and the ring is seeded from it, so
    /// nothing could ever repair it: the node drew a broken picture for the life of the board.
    ///
    /// The three cases are asserted together because the third is only wrong *relative* to
    /// the other two — a rule that turned every unresolved placeholder into an error would
    /// also destroy a hash `serve_image` had already resolved.
    #[test]
    fn a_picture_that_would_not_store_becomes_an_error_rather_than_a_placeholder() {
        let mut names = HashMap::new();
        names.insert("pending:1".to_owned(), "abc123".to_owned());
        let mut failed = HashSet::new();
        failed.insert("pending:2".to_owned());

        let mut stored =
            TranscriptEvent::Image { blob: "pending:1".into(), caption: Some("a chart".into()) };
        resolve_blob(&mut stored, &names, &failed);
        assert_eq!(
            stored,
            TranscriptEvent::Image { blob: "abc123".into(), caption: Some("a chart".into()) },
            "a stored picture did not take its hash"
        );

        let mut refused =
            TranscriptEvent::Image { blob: "pending:2".into(), caption: Some("a chart".into()) };
        resolve_blob(&mut refused, &names, &failed);
        match &refused {
            TranscriptEvent::Error { message } => {
                assert!(message.contains("a chart"), "the error did not name the picture");
            }
            other => panic!("a picture that would not store was recorded as {other:?}"),
        }

        // Neither stored nor refused: a hash `serve_image` already resolved, and a placeholder
        // whose bytes have simply not been offered yet. Both are left exactly as they are.
        let mut settled = TranscriptEvent::Image { blob: "deadbeef".into(), caption: None };
        let before = settled.clone();
        resolve_blob(&mut settled, &names, &failed);
        assert_eq!(settled, before, "a resolved hash was rewritten");

        let mut waiting = TranscriptEvent::Image { blob: "pending:9".into(), caption: None };
        let before = waiting.clone();
        resolve_blob(&mut waiting, &names, &failed);
        assert_eq!(waiting, before, "a picture still in flight was declared lost");
    }

    /// Clicking an option card twice used to send twice: nothing ever set `chosen`, so
    /// `PlateTone::Chosen` was unreachable and every press looked like the first one.
    #[test]
    fn answering_a_question_is_recorded_once_and_refused_the_second_time() {
        let dir = scratch();
        let mut runtime = open(&dir);
        let key = NodeKey::new(&board(), "5@1");
        let choices = vec![Choice::new("a", "Blue"), Choice::new("b", "Green")];
        runtime.record(
            &key,
            1,
            &TranscriptEvent::Options {
                prompt: "which?".into(),
                choices: choices.clone(),
                chosen: None,
            },
        );

        assert!(runtime.choose_option(&key, "a"), "the first press found nothing to answer");
        assert!(!runtime.choose_option(&key, "a"), "a second press answered the same question");
        assert!(!runtime.choose_option(&key, "b"), "a second card answered a settled question");
        assert!(!runtime.choose_option(&key, "nonsense"), "an unknown choice was accepted");

        let state = runtime.nodes.get(&key).expect("the node exists");
        match state.events.back().map(std::convert::AsRef::as_ref) {
            Some(TranscriptEvent::Options { chosen, .. }) => {
                assert_eq!(chosen.as_deref(), Some("a"));
            }
            other => panic!("the ring holds {other:?} rather than an answered question"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transport that does nothing, so a `Session` can exist on a machine with no agent
    /// installed. `Session::over` is public for exactly this.
    struct Silent;

    impl vellum_agent::AgentTransport for Silent {
        fn kind(&self) -> vellum_agent::Transport {
            vellum_agent::Transport::Acp
        }

        fn send_prompt(
            &mut self,
            _turn: vellum_agent::TurnId,
            _prompt: &str,
        ) -> vellum_agent::Result<()> {
            Ok(())
        }

        fn cancel(&mut self) -> vellum_agent::Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> vellum_agent::Result<()> {
            Ok(())
        }
    }

    /// **A node the user deleted must take its session with it.**
    ///
    /// The failure this guards is not subtle and has no other way out: a deleted running
    /// agent keeps its process, `dormant()` never becomes true again, the loopback server
    /// never comes down — and the Stop button went with the node, so nothing on screen can
    /// stop it. It is only noticeable here, because the wiring is derived from the document
    /// and a deleted node is simply absent from it.
    ///
    /// The last assertion is the other half: the transcript **survives**. A node can be
    /// absent because the user pressed ⌘Z, and an undo that destroyed the history would be
    /// unrecoverable.
    #[test]
    fn a_node_that_leaves_the_board_takes_its_session_with_it() {
        let dir = scratch();
        let mut runtime = open(&dir);
        let board = board();
        let key = NodeKey::new(&board, "9@1");

        runtime.record(&key, 1, &TranscriptEvent::Text { text: "working".into() });
        let (voice, events) = channel();
        let session = Session::over(Box::new(Silent), voice, events, String::new());
        runtime.sessions.insert(key.clone(), Live { session, hops: 0 });
        assert!(runtime.is_running(&key));
        assert!(!runtime.dormant());

        // A resync that still names the node leaves it alone.
        runtime.set_wiring(
            &board,
            vec![(key.wire(), "Planner".into(), true)],
            Vec::new(),
            vec![(key.wire(), RoleKind::Worker)],
        );
        assert!(runtime.is_running(&key), "a node that is still on the board lost its session");

        // A resync with the node gone releases it.
        runtime.set_wiring(&board, Vec::new(), Vec::new(), Vec::new());
        assert!(!runtime.is_running(&key), "a deleted node left its process running");
        assert!(runtime.dormant(), "the runtime never went quiet again");

        let kept = runtime
            .sidecar()
            .read_all(&board, &key.item)
            .expect("the transcript reads back");
        assert_eq!(kept.records.len(), 1, "releasing a session destroyed its history");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cost guarantee, stated as an assertion rather than as a comment.
    ///
    /// A board with **no** agent nodes must not resync when the generation moves — that is
    /// every keystroke, because `Projection::refresh_item` bumps it, and the fast path exists
    /// precisely so typing does not cost a rebuild. It must still resync when the item count
    /// moves, because that is the only way an agent node can appear.
    #[test]
    fn a_board_with_no_agents_does_not_resync_on_a_keystroke() {
        let plain = BoardStamp {
            path: Some(PathBuf::from("/tmp/x.vellum")),
            key: board(),
            epoch: 10,
            items: 40,
            has_nodes: false,
            has_notes: false,
            has_content: false,
        };
        let here = Some(Path::new("/tmp/x.vellum"));

        assert!(!plain.needs_resync(here, 10, 40), "nothing moved and it resynced anyway");
        assert!(
            !plain.needs_resync(here, 11, 40),
            "a keystroke on a board with no agents cost a walk of the projection"
        );
        assert!(plain.needs_resync(here, 11, 41), "an item appeared and nothing noticed");
        assert!(plain.needs_resync(Some(Path::new("/tmp/other.vellum")), 10, 40));
        assert!(plain.needs_resync(None, 10, 40), "a board with no file read as the same board");

        // A board that *has* agents follows the generation, because a role, a schedule or an
        // arrowhead can change without the count moving.
        let live = BoardStamp { has_nodes: true, has_content: true, ..plain.clone() };
        assert!(live.needs_resync(here, 11, 40));
        assert!(!live.needs_resync(here, 10, 40));

        // And so does one with only notes — a set of file-backed documents on a canvas is an
        // ordinary thing to want, and that board still has files to watch.
        let noted = BoardStamp { has_notes: true, has_content: true, ..plain.clone() };
        assert!(noted.needs_resync(here, 11, 40));

        // And one holding only a file tree or a browser node, which have neither flag above
        // and are still Agent Canvas content that has to be given a view. A board of those
        // used to be treated exactly like a board of stickies: never resynced, never handed a
        // view, so its tree said "not been read yet" for ever.
        let others = BoardStamp { has_content: true, ..plain };
        assert!(
            others.needs_resync(here, 11, 40),
            "a board of file trees and browser nodes never noticed its own edits"
        );
    }

    /// A note listing reads as a set of documents rather than a set of filenames, and a
    /// heading is what makes that true.
    #[test]
    fn a_notes_title_comes_from_its_first_heading() {
        assert_eq!(heading_of("# Plan\n\nbody"), Some("Plan".into()));
        assert_eq!(heading_of("## Deeper\n"), Some("Deeper".into()));
        assert_eq!(heading_of("no heading at all"), None);
        assert_eq!(heading_of("#\n# Real\n"), Some("Real".into()));
        // Arbitrary agent output: bounded by characters, never bytes, or a multi-byte
        // character straddling the boundary aborts the process (feedback 30).
        let long = format!("# {}", "夕".repeat(400));
        assert_eq!(heading_of(&long).map(|t| t.chars().count()), Some(80));
    }
}
