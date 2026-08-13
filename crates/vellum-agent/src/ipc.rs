//! The loopback server an agent's own process talks back to Velm through.
//!
//! An agent runs in a child process — `claude`, `codex`, a PTY, a model on the user's GPU —
//! and cannot call into the application that placed it on the board. So §6 gives it a socket
//! and a shim: agents are launched with `VELM_IPC` and `VELM_AGENT_ID` in their environment
//! and `velm-agent-cli` on their `PATH`, and every verb in this file is something the agent
//! can *do to the board* from inside its own sandbox — message a neighbour, read a note,
//! spawn a helper, post a picture, offer the user a choice, or (for the meta agent alone)
//! rewrite another node's configuration.
//!
//! # The one place a mistake exposes the user's machine
//!
//! The listener binds **`127.0.0.1:0`**: the loopback interface, an ephemeral port, and
//! **never a wildcard bind**. `0.0.0.0` would offer the whole verb surface — including
//! *rewrite that agent's configuration* and *spawn a process* — to anything that could reach
//! the machine on the network. There is one assertion pinning this
//! (`the_server_binds_loopback_and_an_ephemeral_port`), and it exists precisely so that a
//! later edit changing the bind address fails a test rather than shipping.
//!
//! The port and a **per-launch random token** are written to `<data-dir>/runtime/ipc.json`
//! with mode `0600`. The token is 32 bytes read from `/dev/urandom` — not a time-seeded
//! PRNG, because a predictable token on a port every local process can reach is a local
//! privilege problem rather than a theoretical one.
//!
//! # What the token proves, and what it does not
//!
//! One token per launch, shared by every agent Velm starts. So the token authenticates **the
//! process tree**, not the individual node: any process holding it can claim any
//! `VELM_AGENT_ID`, and [`IpcHandler::role_of`]'s answer is therefore a statement about the
//! id that was *claimed*. That is trust *within* the boundary, and it is written down here
//! rather than left for a reviewer to find. The hardening step, when it is worth its
//! complexity, is a token per node handed to that node's process alone; nothing about the
//! wire format changes when it lands.
//!
//! # The capability boundary is here, not in a prompt
//!
//! `configure` is refused for anything but [`RoleKind::Meta`], and `spawn` for anything that
//! is not an orchestrator or the meta agent — **checked in `dispatch` before the handler is
//! called**. A capability that lives only in an instruction is not a capability boundary: an
//! agent told in words not to reconfigure its neighbours is an agent that eventually will,
//! and the same reasoning already puts an orchestrator's spawn cap in Velm rather than in its
//! system context (§9).
//!
//! # The seam
//!
//! This module owns the socket, the framing, the authentication and the thread. It owns
//! nothing about what a verb *does*. [`IpcHandler`] is the seam: `vellum-app` implements it,
//! typically as a channel pair drained once per frame — the `links.rs` shape — so the verbs
//! run on the thread that owns the document and this file never touches a board.
//!
//! # Idle cost
//!
//! Nothing here exists until an agent node does. [`IpcServer::start`] is called when the
//! first one is placed and [`IpcServer::stop`] when the last one goes; a board without agents
//! opens no socket, writes no runtime file and starts no thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{AgentModel, NoteScope, RoleKind};
use crate::transcript::Choice;
use crate::AgentError;

/// The longest single request the server will read.
///
/// A line-delimited protocol with no bound is a memory sink reachable by anything that can
/// open a loopback socket: one client sending bytes without a newline would grow a `String`
/// until the machine gave up. 32 MiB clears the largest thing a verb legitimately carries —
/// a screenshot, base64'd, which inflates by a third — with room to spare.
pub const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;

/// How many requests one connection may make before the server closes it.
///
/// The accept loop is a single thread serving connections in turn (§6: *one thread, blocking
/// accept*), which is right for a shim that connects, asks one thing and exits. The cost is
/// that a client which never lets go would monopolise it, so a connection gets a generous
/// budget and then a polite close. A long-lived client is the signal to spawn per connection;
/// there is no such client today.
const MAX_REQUESTS_PER_CONNECTION: u32 = 64;

/// How long the server waits for a client that has connected and gone quiet.
const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a client waits to reach Velm at all.
///
/// Short, because failing to connect means the app is not running and no amount of waiting
/// changes that. The shim runs *inside an agent's tool call*: a client with no timeout does
/// not fail, it hangs the agent.
pub const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a client waits for an answer once connected.
///
/// Generous, because a verb is served by the application's frame loop and may wait a frame or
/// two behind whatever else it was doing — but finite, for the same reason.
pub const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------
// The runtime file
// ---------------------------------------------------------------------------------------

/// What is written to `<data-dir>/runtime/ipc.json` so a shim can find the server.
///
/// Deliberately not in the board file, not in the library sidecar and not in any settings:
/// it is true only while this process is running, and a stale copy in a durable file would be
/// a port number pointing at nothing.
/// **Never logged.** The `Debug` impl prints the port and the pid and not one byte of the
/// token — see the impl below for why a derive here is a hole rather than a nicety.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFile {
    pub port: u16,
    /// The per-launch token. **Never logged, never printed, never put in a toast.**
    pub token: String,
    /// The process that wrote it. Diagnostic only — it is never consulted for authentication,
    /// because a pid is not a secret and reusing one as a credential would be a hole rather
    /// than a check. It is here so a human debugging a stale file can tell whether the
    /// process that wrote it is still alive.
    pub pid: u32,
}

/// ⚠ **Hand-written, and the field it hides is the one the whole IPC surface is protected by.**
///
/// The token authorises every verb a shim can ask for — send a message as another agent, write
/// a note, spawn a sub-agent — and this file is written mode `0600` precisely because it is a
/// credential. `#[derive(Debug)]` undoes that at the first `dbg!` anybody writes, in any panic
/// message that formats a struct holding one, and in every `assert_eq!` failure in this
/// module's own tests — which is where it would first be read out loud.
///
/// Latent today: nothing formats one. That is exactly the argument the derive would win on,
/// and it is wrong — this crate hand-writes redacting impls for `Credentials`, `ClaudeCli` and
/// `AcpTransport` for the same reason, and a secret's safety should not depend on nobody
/// having reached for the obvious debugging tool yet.
impl std::fmt::Debug for RuntimeFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeFile")
            .field("port", &self.port)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl RuntimeFile {
    /// Where the file lives under a given data directory.
    ///
    /// One definition, so the app writing it and the shim being told about it cannot come to
    /// disagree about the path.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("runtime").join("ipc.json")
    }

    pub fn read(path: &Path) -> crate::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// The server's address on this machine. Always loopback — the file carries a port, not a
    /// host, so a runtime file cannot point a shim at another machine even if one were
    /// planted.
    pub fn address(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }

    /// Make one request and read the answer. The shim's whole job, and the app's own way of
    /// checking the loop end to end.
    pub fn call(&self, agent: &str, request: Request) -> crate::Result<Response> {
        call(
            self.address(),
            &Envelope { token: self.token.clone(), agent: agent.to_owned(), request },
        )
    }
}

/// Send one envelope to a server and read one response.
///
/// Both timeouts are mapped to an ordinary [`AgentError::Io`], so a caller distinguishes
/// *"Velm is not reachable"* from *"Velm answered no"* by which side of the `Result` it is on
/// rather than by parsing a message.
pub fn call(address: SocketAddr, envelope: &Envelope) -> crate::Result<Response> {
    let stream = TcpStream::connect_timeout(&address, CLIENT_CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_READ_TIMEOUT))?;

    let mut writer = stream.try_clone()?;
    let line = serde_json::to_string(envelope)?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    if reader.read_line(&mut answer)? == 0 {
        return Err(AgentError::Transport {
            transport: "ipc",
            message: "Velm closed the connection without answering".into(),
        });
    }
    Ok(serde_json::from_str(answer.trim_end())?)
}

// ---------------------------------------------------------------------------------------
// The wire format
// ---------------------------------------------------------------------------------------

/// One request, as it goes down the wire: who is asking, with what credential, for what.
///
/// The verb lives in a **nested** object rather than being flattened alongside `token` and
/// `agent`. Flattening would read slightly better and would mean a verb could one day carry a
/// field called `token` and shadow the one that authenticates it — a wire format where the
/// credential and the payload share a namespace is one substitution away from being wrong.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// The per-launch token from the runtime file.
    pub token: String,
    /// The calling agent's node id, from `VELM_AGENT_ID`.
    pub agent: String,
    pub request: Request,
}

/// ⚠ **Hand-written, for the same reason [`RuntimeFile`]'s is** — and this one is the more
/// exposed of the two, because an `Envelope` is a *message*: it is what a failed request holds
/// when something goes wrong and somebody reaches for `dbg!` to find out what was on the wire.
/// The verb is printed, which is the useful half; the credential is not.
impl std::fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Envelope")
            .field("agent", &self.agent)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// What an agent may ask Velm to do.
///
/// These are exactly the verbs `velm-agent-cli` exposes and exactly the tools `velm-mcp`
/// will, so an agent that speaks MCP and one that shells out get the same surface — and one
/// enum is what makes that true rather than aspirational.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb")]
pub enum Request {
    /// Message another agent along a connector. `to` is a node id or the label on the node;
    /// see [`crate::bus::Topology::resolve`] for why an ambiguous label is refused.
    #[serde(rename = "send")]
    Send { to: String, text: String },

    /// Read a note's markdown.
    #[serde(rename = "note.read")]
    NoteRead { path: String },

    /// Write a note. `append` is the difference between a scratchpad and a report, and both
    /// are things agents do constantly; without it every append is a read-modify-write race
    /// against the user's own editor.
    #[serde(rename = "note.write")]
    NoteWrite {
        path: String,
        text: String,
        #[serde(default)]
        append: bool,
    },

    /// Every note this agent may see — shared notes on the board, plus its own private ones.
    #[serde(rename = "note.list")]
    NoteList,

    /// Ask for a sub-agent. Territory and cap are enforced by `orchestrator.rs`; this carries
    /// the request there and the refusal back.
    #[serde(rename = "spawn")]
    Spawn(SpawnRequest),

    /// Post a picture into this agent's transcript. The app hashes the bytes into the
    /// existing blob store, so a picture posted twice costs one copy (§4).
    #[serde(rename = "image")]
    Image {
        /// Standard base64 of the encoded image file — PNG, JPEG, whatever the agent made.
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },

    /// Offer the user a set of options — §11's row of cards, the thing a chat window cannot
    /// do. Clicking one sends that choice back as the next turn's input.
    #[serde(rename = "options")]
    Options { prompt: String, choices: Vec<Choice> },

    /// Read or rewrite another node's configuration. **Meta agent only**, enforced by the
    /// server before the handler is reached.
    ///
    /// `model: None` reads; `Some` replaces. Whole-model rather than a patch, deliberately:
    /// a patch language is a second schema that has to track [`AgentModel`] field for field,
    /// and the read-edit-write the meta agent does instead is also what lets it say what it
    /// changed.
    #[serde(rename = "configure")]
    Configure {
        node: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<Box<AgentModel>>,
    },
}

impl Request {
    /// The verb's name, for a refusal log and for the CLI's own messages.
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::NoteRead { .. } => "note.read",
            Self::NoteWrite { .. } => "note.write",
            Self::NoteList => "note.list",
            Self::Spawn(_) => "spawn",
            Self::Image { .. } => "image",
            Self::Options { .. } => "options",
            Self::Configure { .. } => "configure",
        }
    }
}

/// What an orchestrator is asking for when it spawns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SpawnRequest {
    /// The label to put on the new node. An orchestrator that spawns five agents called
    /// nothing has made a board nobody can read.
    pub label: String,
    /// What the new agent is. Defaults to a worker: an orchestrator spawning orchestrators is
    /// how a cap gets multiplied rather than applied.
    #[serde(default)]
    pub role: RoleKind,
    /// The first thing to ask it, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Where to put it, in world units. `None` lets the app place it inside the
    /// orchestrator's territory, which is the only placement that is always legal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<(f64, f64)>,
}

/// One note an agent can see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteEntry {
    /// The path, as the note node stores it — relative to the project where there is one.
    /// This is the string `note.read` and `note.write` take.
    pub path: String,
    pub scope: NoteScope,
    /// The note's title, when it has one, so a listing reads as a set of documents rather
    /// than a set of filenames.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// A machine-readable reason a request was refused.
///
/// Separate from the message so the CLI can exit with a code that means something and an MCP
/// client can branch, without either of them matching on English prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The token did not match. The only refusal that says nothing else, on purpose.
    BadToken,
    /// `VELM_AGENT_ID` does not name an agent node on any open board.
    UnknownAgent,
    /// No connector joins these two agents. See §3.
    NotConnected,
    /// The request was understood and refused — a capability, a cap, a territory, a mute.
    Refused,
    /// The line was not a request this build understands.
    Malformed,
    /// It was attempted and it failed — a file that would not open, a transport that died.
    Failed,
}

impl ErrorCode {
    /// The refusal a crate error becomes on the wire.
    fn of(error: &AgentError) -> Self {
        match error {
            AgentError::NotConnected { .. } => Self::NotConnected,
            AgentError::Refused(_) => Self::Refused,
            AgentError::Json(_) => Self::Malformed,
            _ => Self::Failed,
        }
    }
}

/// What a verb answered with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    /// It was done and there is nothing to report.
    Done,
    /// A note's contents.
    Note { text: String },
    /// Every note this agent can see.
    Notes { notes: Vec<NoteEntry> },
    /// The id of the agent that was created.
    Spawned { agent: String },
    /// A node's configuration.
    Config { model: Box<AgentModel> },
}

/// One response, one line of JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Answer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

impl Response {
    pub fn done() -> Self {
        Self::ok(Answer::Done)
    }

    pub fn ok(answer: Answer) -> Self {
        Self { ok: true, answer: Some(answer), error: None, code: None }
    }

    pub fn refused(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { ok: false, answer: None, error: Some(message.into()), code: Some(code) }
    }

    fn from_error(error: &AgentError) -> Self {
        Self::refused(ErrorCode::of(error), error.to_string())
    }

    /// The message, or a stand-in — for a client printing a failure. Never `unwrap`, because
    /// a malformed response from a future build must still produce a sentence.
    pub fn message(&self) -> &str {
        self.error.as_deref().unwrap_or("Velm refused the request without saying why")
    }
}

// ---------------------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------------------

/// What the application supplies. The server owns the socket; this owns the meaning.
///
/// Implemented by `vellum-app`, and it must be `Send + Sync` because it is called from the
/// accept thread. The expected shape is **not** to do the work there: a real implementation
/// puts the request on a channel, the frame loop serves it against the live document and
/// answers on a reply channel, and this trait's method blocks on that one round trip. That
/// keeps every board mutation on the thread that owns the document, which is the same rule
/// `links.rs` follows for fetched previews.
///
/// Every method has a default that **refuses by name**, so a partially wired app answers
/// *"note.write is not wired up in this build yet"* rather than tempting somebody to write a
/// stub that silently succeeds. [`IpcHandler::role_of`] is the exception and has no default:
/// it is the boundary check, and a default for it would be a decision about who may
/// reconfigure the board made by absent-mindedness.
pub trait IpcHandler: Send + Sync + 'static {
    /// The role of the node claiming to be `agent`, or `None` if no such node is open.
    ///
    /// Called on **every** request, before the verb runs, so an id that names nothing is
    /// refused once here rather than in eight handlers.
    fn role_of(&self, agent: &str) -> Option<RoleKind>;

    /// Route a message. `to` is whatever the agent typed — an id or a label — and the
    /// implementation is expected to resolve it through [`crate::bus::Topology::resolve`] and
    /// hand it to [`crate::bus::Bus::send`], which is where the connector rule and the hop
    /// bound live.
    fn send_message(&self, from: &str, to: &str, text: &str) -> crate::Result<()> {
        let _ = (from, to, text);
        Err(unwired("send"))
    }

    fn note_read(&self, agent: &str, path: &str) -> crate::Result<String> {
        let _ = (agent, path);
        Err(unwired("note.read"))
    }

    fn note_write(
        &self,
        agent: &str,
        path: &str,
        text: &str,
        append: bool,
    ) -> crate::Result<()> {
        let _ = (agent, path, text, append);
        Err(unwired("note.write"))
    }

    fn note_list(&self, agent: &str) -> crate::Result<Vec<NoteEntry>> {
        let _ = agent;
        Err(unwired("note.list"))
    }

    /// Create a sub-agent, or refuse. The role check has already been made; the **cap and the
    /// territory have not**, and this is where they are applied.
    fn spawn(&self, parent: &str, request: &SpawnRequest) -> crate::Result<String> {
        let _ = (parent, request);
        Err(unwired("spawn"))
    }

    /// Put a picture in the agent's transcript. The bytes are the encoded file; the app hashes
    /// them into the blob store and records the hash.
    fn post_image(&self, agent: &str, bytes: &[u8], caption: Option<&str>) -> crate::Result<()> {
        let _ = (agent, bytes, caption);
        Err(unwired("image"))
    }

    fn post_options(
        &self,
        agent: &str,
        prompt: &str,
        choices: &[Choice],
    ) -> crate::Result<()> {
        let _ = (agent, prompt, choices);
        Err(unwired("options"))
    }

    /// Read a node's configuration. Only ever reached by the meta agent.
    fn read_config(&self, node: &str) -> crate::Result<AgentModel> {
        let _ = node;
        Err(unwired("configure"))
    }

    /// Replace a node's configuration. Only ever reached by the meta agent.
    fn write_config(&self, node: &str, model: AgentModel) -> crate::Result<()> {
        let _ = (node, model);
        Err(unwired("configure"))
    }

    /// A request was refused. Overridden by the app to reach the flight recorder; the default
    /// is stderr, because a refusal nobody records is a security event nobody can investigate.
    ///
    /// `agent` is the id that was **claimed**, which for a bad token is a string a stranger
    /// chose. It is recorded as-is and read as a claim.
    fn log_refusal(&self, agent: &str, verb: &str, reason: &str) {
        eprintln!("velm ipc: refused {verb} from {agent}: {reason}");
    }
}

fn unwired(verb: &str) -> AgentError {
    AgentError::Refused(format!("{verb} is not wired up in this build yet"))
}

// ---------------------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------------------

/// A running loopback server. Dropping it stops it.
pub struct IpcServer {
    address: SocketAddr,
    token: String,
    runtime_path: PathBuf,
    stopping: Arc<AtomicBool>,
    refusals: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind, write the runtime file, and start serving.
    ///
    /// The order is load-bearing: the file is written **after** the bind succeeds and carries
    /// the port the OS actually gave us. Writing it first would advertise a port nothing is
    /// listening on, and every shim invocation for the rest of the session would fail against
    /// a file that looked perfectly valid.
    pub fn start(data_dir: &Path, handler: Arc<dyn IpcHandler>) -> crate::Result<Self> {
        // Loopback, ephemeral port. Never a wildcard — see the module header.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;

        let token = random_token()?;
        let runtime_path = RuntimeFile::path_in(data_dir);
        let file = RuntimeFile {
            port: address.port(),
            token: token.clone(),
            pid: std::process::id(),
        };
        write_private_json(&runtime_path, &serde_json::to_string(&file)?)?;

        let stopping = Arc::new(AtomicBool::new(false));
        let refusals = Arc::new(AtomicU64::new(0));

        let thread = {
            let stopping = Arc::clone(&stopping);
            let refusals = Arc::clone(&refusals);
            let token = token.clone();
            std::thread::Builder::new()
                .name("velm-ipc".into())
                .spawn(move || serve(&listener, handler.as_ref(), &token, &stopping, &refusals))?
        };

        Ok(Self { address, token, runtime_path, stopping, refusals, thread: Some(thread) })
    }

    /// Where it is listening. Loopback, always.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// The per-launch token.
    ///
    /// Exists so the app can put it in a child process's environment and so tests can make a
    /// legitimate request. **It is never printed, logged or shown**; the shim reads it from
    /// the runtime file and does not echo it either.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    /// How many requests have been refused. For the HUD, and for a test that needs to know a
    /// refusal was counted rather than swallowed.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    /// Stop serving, join the thread and remove the runtime file. Idempotent.
    ///
    /// A blocking `accept` cannot be interrupted portably, so the flag is set and then **one
    /// connection is made to ourselves** purely to wake it; the loop checks the flag before
    /// serving anything and breaks. Joining matters: without it a test that starts and stops
    /// servers leaks threads that are still holding ports.
    ///
    /// # ⚠ The one way this deadlocks
    ///
    /// An [`IpcHandler`] is expected to block on a round trip with the frame loop. If the
    /// **frame loop itself** calls `stop` while the server thread is waiting on it, the join
    /// waits for a thread that is waiting for the caller. Two ways out, and the app must take
    /// one: serve the handler's channel until the join returns, or answer every in-flight
    /// request with a refusal before stopping. Stopping from a thread that is not the one
    /// serving [`IpcHandler`] is not affected.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stopping.store(true, Ordering::SeqCst);
        // Wake the accept. The loop breaks on the flag before reading anything from it.
        let _ = TcpStream::connect_timeout(&self.address, CLIENT_CONNECT_TIMEOUT);
        let _ = thread.join();
        self.remove_runtime_file();
    }

    /// Remove the runtime file **only if it is still ours**.
    ///
    /// A newer server may already have written its own over the top — which happens in tests
    /// constantly and would happen in the app the moment a second window opened. Deleting
    /// somebody else's runtime file would leave a live server that no shim could find.
    fn remove_runtime_file(&self) {
        let ours = RuntimeFile::read(&self.runtime_path)
            .is_ok_and(|file| file.port == self.address.port() && file.token == self.token);
        if ours {
            let _ = std::fs::remove_file(&self.runtime_path);
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(
    listener: &TcpListener,
    handler: &dyn IpcHandler,
    token: &str,
    stopping: &AtomicBool,
    refusals: &AtomicU64,
) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        match stream {
            Ok(stream) => serve_connection(&stream, handler, token, refusals),
            // A failed accept is not a reason to stop answering: the next client may be fine.
            Err(_) => continue,
        }
    }
}

fn serve_connection(
    stream: &TcpStream,
    handler: &dyn IpcHandler,
    token: &str,
    refusals: &AtomicU64,
) {
    let _ = stream.set_read_timeout(Some(SERVER_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SERVER_WRITE_TIMEOUT));

    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reading);
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };

    for _ in 0..MAX_REQUESTS_PER_CONNECTION {
        let mut line = String::new();
        // Bounded: `Read::take` stops at MAX_REQUEST_BYTES whether or not a newline ever
        // arrives, which is what keeps an unterminated line from growing without limit.
        let read = (&mut reader).take(MAX_REQUEST_BYTES).read_line(&mut line);
        let response = match read {
            // A clean close.
            Ok(0) => return,
            Ok(n) if n as u64 >= MAX_REQUEST_BYTES && !line.ends_with('\n') => Response::refused(
                ErrorCode::Malformed,
                format!("a request may not exceed {MAX_REQUEST_BYTES} bytes"),
            ),
            Ok(_) => match serde_json::from_str::<Envelope>(line.trim_end()) {
                Ok(envelope) => dispatch(envelope, handler, token, refusals),
                // **Answered, not hung up on.** A malformed line is a bug in one call, and
                // closing the connection would make it look like Velm had crashed — which is
                // exactly the wrong thing to tell an agent that is about to retry.
                Err(error) => Response::refused(
                    ErrorCode::Malformed,
                    format!("that was not a request Velm understands: {error}"),
                ),
            },
            // A timeout or a dropped peer. Nothing to answer to.
            Err(_) => return,
        };

        let Ok(encoded) = serde_json::to_string(&response) else {
            return;
        };
        if writer.write_all(encoded.as_bytes()).is_err()
            || writer.write_all(b"\n").is_err()
            || writer.flush().is_err()
        {
            return;
        }
    }
}

/// Authenticate, check the capability, then run the verb.
///
/// The two checks that happen **here rather than in the handler** are the token and the role.
/// Both are boundary conditions: an implementation of [`IpcHandler`] that forgot either would
/// still compile, still pass its own tests, and still hand the board's control plane to a
/// worker.
fn dispatch(
    envelope: Envelope,
    handler: &dyn IpcHandler,
    token: &str,
    refusals: &AtomicU64,
) -> Response {
    let Envelope { token: offered, agent, request } = envelope;
    let verb = request.verb();

    let refuse = |code: ErrorCode, message: String| -> Response {
        refusals.fetch_add(1, Ordering::Relaxed);
        handler.log_refusal(&agent, verb, &message);
        Response::refused(code, message)
    };

    if !secrets_match(&offered, token) {
        // Says nothing else — not whether the agent exists, not whether the verb is real.
        return refuse(ErrorCode::BadToken, "that token is not this session's".into());
    }

    let Some(role) = handler.role_of(&agent) else {
        return refuse(
            ErrorCode::UnknownAgent,
            format!("\"{agent}\" is not an agent node on any open board"),
        );
    };

    // The capability boundary. §6 and `RoleKind::may_configure_others`.
    if matches!(request, Request::Configure { .. }) && !role.may_configure_others() {
        return refuse(
            ErrorCode::Refused,
            format!(
                "only a meta agent may read or rewrite another node's configuration; \
                 {agent} is set to \"{}\"",
                role.label()
            ),
        );
    }
    if matches!(request, Request::Spawn(_)) && !role.may_spawn() {
        return refuse(
            ErrorCode::Refused,
            format!(
                "only an orchestrator or a meta agent may spawn agents; \
                 {agent} is set to \"{}\"",
                role.label()
            ),
        );
    }

    let answered = match request {
        Request::Send { to, text } => {
            handler.send_message(&agent, &to, &text).map(|()| Answer::Done)
        }
        Request::NoteRead { path } => {
            handler.note_read(&agent, &path).map(|text| Answer::Note { text })
        }
        Request::NoteWrite { path, text, append } => handler
            .note_write(&agent, &path, &text, append)
            .map(|()| Answer::Done),
        Request::NoteList => handler.note_list(&agent).map(|notes| Answer::Notes { notes }),
        Request::Spawn(request) => {
            handler.spawn(&agent, &request).map(|agent| Answer::Spawned { agent })
        }
        Request::Image { data, caption } => match decode_base64(&data) {
            Some(bytes) => handler
                .post_image(&agent, &bytes, caption.as_deref())
                .map(|()| Answer::Done),
            None => {
                return refuse(
                    ErrorCode::Malformed,
                    "the image data was not valid base64".into(),
                );
            }
        },
        Request::Options { prompt, choices } => handler
            .post_options(&agent, &prompt, &choices)
            .map(|()| Answer::Done),
        Request::Configure { node, model } => match model {
            Some(model) => handler
                .write_config(&node, *model)
                .map(|()| Answer::Done),
            None => handler
                .read_config(&node)
                .map(|model| Answer::Config { model: Box::new(model) }),
        },
    };

    match answered {
        Ok(answer) => Response::ok(answer),
        Err(error) => {
            refusals.fetch_add(1, Ordering::Relaxed);
            handler.log_refusal(&agent, verb, &error.to_string());
            Response::from_error(&error)
        }
    }
}

/// Compare two secrets without an early return on the first differing byte.
///
/// Cheap insurance rather than a claim: a timing attack against a loopback port by a process
/// that could simply read the runtime file is not the threat here. It costs four lines and
/// removes the question.
fn secrets_match(offered: &str, expected: &str) -> bool {
    if offered.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in offered.bytes().zip(expected.bytes()) {
        difference |= a ^ b;
    }
    difference == 0
}

// ---------------------------------------------------------------------------------------
// The token, and the file it is written to
// ---------------------------------------------------------------------------------------

/// 32 bytes of OS entropy, hex encoded.
///
/// `read_exact`, not `read`: a short read from `/dev/urandom` is legal and would silently
/// leave the tail of the token as zeroes.
#[cfg(unix)]
fn random_token() -> std::io::Result<String> {
    let mut source = std::fs::File::open("/dev/urandom")?;
    let mut bytes = [0u8; 32];
    source.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

/// The Windows stand-in, and an honest account of what it is.
///
/// `RandomState`'s keys come from the operating system's randomness at process start — it is
/// **not** a time-seeded PRNG, which is the thing that must not be used here — and hashing
/// four distinct inputs under one such key yields 256 bits derived from ~128 bits of OS
/// entropy. That is ample for a loopback token and it is less than `/dev/urandom` gives.
///
/// The right fix is `BCryptGenRandom` through `windows-sys`, which is a dependency this crate
/// does not carry today; it is worth adding when Velm's Windows build is actually exercised,
/// and the token's shape does not change when it lands.
#[cfg(not(unix))]
fn random_token() -> std::io::Result<String> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let state = RandomState::new();
    let mut bytes = [0u8; 32];
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_u64(index as u64);
        hasher.write_u32(std::process::id());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Write a file only this user can read, atomically.
///
/// Temp-and-rename for the reason §8 gives for notes: a shim reading the file while it is
/// being written must never see half of one. The temp file is created with the restrictive
/// mode and `rename` preserves it, so there is no window where the token is world-readable.
fn write_private_json(path: &Path, contents: &str) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentError::file(parent.display().to_string(), &error))?;
        tighten_directory(parent);
    }
    let temp = path.with_extension("tmp");
    {
        let mut file = private_options()
            .open(&temp)
            .map_err(|error| AgentError::file(temp.display().to_string(), &error))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| AgentError::file(temp.display().to_string(), &error))?;
        file.sync_all()
            .map_err(|error| AgentError::file(temp.display().to_string(), &error))?;
    }
    std::fs::rename(&temp, path)
        .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
    Ok(())
}

#[cfg(unix)]
fn private_options() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    options
}

/// On Windows there is no `mode`, and the data directory is already per user. The file is
/// created the ordinary way and inherits that.
#[cfg(not(unix))]
fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    options
}

/// Best effort: the runtime directory is the agent's, not the world's. Failure is ignored —
/// a directory that cannot be chmod'd is not a reason to refuse to run agents, and the file
/// inside it carries its own mode regardless.
#[cfg(unix)]
fn tighten_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn tighten_directory(_path: &Path) {}

// ---------------------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------------------

const ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648 §4), padded.
///
/// Encoding lives here rather than behind a crate because the wire format has to be pinned by
/// test vectors either way, and these fifty lines are the whole of it. The vectors in
/// `base64_matches_rfc_4648_and_survives_a_round_trip` are the specification's own, so
/// swapping in a library later cannot change a byte.
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let packed = (first << 16) | (second << 8) | third;

        out.push(ALPHABET[(packed >> 18) as usize & 63] as char);
        out.push(ALPHABET[(packed >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(packed >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { ALPHABET[packed as usize & 63] as char } else { '=' });
    }
    out
}

/// The other direction, and it is **strict**. `None` for anything [`encode_base64`] would not
/// have produced **from bytes worth carrying** — which is every string it produces except the
/// one it makes from nothing.
///
/// # Why strict, when lenient is friendlier
///
/// Trap 2 in `CLAUDE.md` is this exact mistake at a different layer: Miro's clipboard payload
/// was decoded leniently, the closing delimiter went into the decoder, and the corruption was
/// invisible for a long time because *something* always came out. This decoder is on the path
/// that carries **an image an agent posted** — bytes that go straight into the content-addressed
/// blob store, where a wrong byte becomes a wrong hash and a picture nobody can explain.
/// Something that always decodes is a decoder that never reports a bug.
///
/// So four things are refused rather than tolerated, and each of them is a *near miss* — the
/// shape a mistake takes, not the shape rubbish takes:
///
/// - **A length that is not a multiple of four.** That is a truncated payload, which is what a
///   dropped write or a clipped argument list produces.
/// - **`=` anywhere but as one or two trailing characters.** A `=` in the middle is two
///   payloads spliced together.
/// - **Whitespace, including newlines.** Every producer of this wire format is
///   [`encode_base64`], which never emits any; a newline in the data field means the framing
///   went wrong, and this is a line-delimited protocol where that matters.
/// - **Non-canonical trailing bits.** `Zg==` and `Zh==` differ, and a decoder that discards
///   the tail says they are the same byte. Two spellings of one payload is how a
///   content-addressed store gets two hashes for one picture.
/// - **The empty string**, which is the only refusal here that is not a malformed input. It is
///   perfectly good base64 for zero bytes, and zero bytes is not a thing any of this
///   function's three callers can use: all three are decoding **an image an agent posted**,
///   straight into the blob store. Refusing `"a"` and accepting `""` left the *same* zero-byte
///   blob reachable by the honest route, with two test files asserting it was correct. The
///   check belongs here rather than at the callers precisely because there are three of them
///   and a rule applied at two of three is how this defect got its second life.
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }

    // Padding is only ever the last one or two characters. Counting from the end and then
    // refusing `=` in the body is what makes a `=` in the middle a refusal rather than a skip.
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return None;
    }
    let body = &bytes[..bytes.len() - padding];

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in body {
        let value: u32 = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            // Everything else, `=` and whitespace included. There is no `continue` arm here
            // on purpose: a skipped character is a silently different payload.
            _ => return None,
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }

    // The bits left over must be the zeroes the encoder padded with.
    if bits > 0 && accumulator & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A stand-in for `vellum-app`: it records what it was asked and answers.
    struct TestApp {
        roles: HashMap<String, RoleKind>,
        sent: Mutex<Vec<(String, String, String)>>,
        configured: Mutex<Vec<String>>,
        images: Mutex<Vec<Vec<u8>>>,
        refusals: Mutex<Vec<String>>,
    }

    impl TestApp {
        fn new() -> Arc<Self> {
            let mut roles = HashMap::new();
            roles.insert("worker".to_owned(), RoleKind::Worker);
            roles.insert("boss".to_owned(), RoleKind::Orchestrator);
            roles.insert("meta".to_owned(), RoleKind::Meta);
            Arc::new(Self {
                roles,
                sent: Mutex::new(Vec::new()),
                configured: Mutex::new(Vec::new()),
                images: Mutex::new(Vec::new()),
                refusals: Mutex::new(Vec::new()),
            })
        }
    }

    impl IpcHandler for TestApp {
        fn role_of(&self, agent: &str) -> Option<RoleKind> {
            self.roles.get(agent).copied()
        }

        fn send_message(&self, from: &str, to: &str, text: &str) -> crate::Result<()> {
            self.sent.lock().unwrap().push((
                from.to_owned(),
                to.to_owned(),
                text.to_owned(),
            ));
            Ok(())
        }

        fn note_read(&self, _agent: &str, path: &str) -> crate::Result<String> {
            Ok(format!("# {path}\n"))
        }

        fn post_image(
            &self,
            _agent: &str,
            bytes: &[u8],
            _caption: Option<&str>,
        ) -> crate::Result<()> {
            self.images.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }

        fn read_config(&self, _node: &str) -> crate::Result<AgentModel> {
            Ok(AgentModel::worker())
        }

        fn write_config(&self, node: &str, _model: AgentModel) -> crate::Result<()> {
            self.configured.lock().unwrap().push(node.to_owned());
            Ok(())
        }

        fn log_refusal(&self, agent: &str, verb: &str, reason: &str) {
            self.refusals.lock().unwrap().push(format!("{agent}/{verb}: {reason}"));
        }
    }

    /// One connection, held open across several requests — which is what makes the
    /// "malformed JSON does not kill the connection" assertion possible at all.
    struct Client {
        writer: TcpStream,
        reader: BufReader<TcpStream>,
    }

    impl Client {
        fn connect(address: SocketAddr) -> Self {
            let stream = TcpStream::connect(address).expect("connect");
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let reader = BufReader::new(stream.try_clone().unwrap());
            Self { writer: stream, reader }
        }

        /// Send a raw line and read the raw answer. Raw on purpose: these tests are about the
        /// wire format, and building an `Envelope` with the same serialiser the server uses
        /// would agree with itself no matter what it emitted.
        fn raw(&mut self, line: &str) -> Response {
            self.writer.write_all(line.as_bytes()).unwrap();
            self.writer.write_all(b"\n").unwrap();
            self.writer.flush().unwrap();
            let mut answer = String::new();
            self.reader.read_line(&mut answer).expect("no answer");
            serde_json::from_str(answer.trim_end())
                .unwrap_or_else(|error| panic!("unreadable answer {answer:?}: {error}"))
        }
    }

    fn start(app: &Arc<TestApp>) -> (IpcServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        // The unsizing `Arc<TestApp>` -> `Arc<dyn IpcHandler>` happens at a *coercion
        // site*, and a generic parameter is not one — so `Arc::clone(app)` passed straight
        // in infers `Arc<TestApp>` and fails. The annotated `let` is the coercion site.
        let handler: Arc<dyn IpcHandler> = app.clone();
        let server = IpcServer::start(dir.path(), handler).expect("start");
        (server, dir)
    }

    /// **The assertion that catches a wildcard bind.** If somebody later changes the bind to
    /// `0.0.0.0` — for a remote agent, for a container, for any perfectly reasonable-sounding
    /// reason — this fails, and the whole verb surface stays off the network.
    #[test]
    fn the_server_binds_loopback_and_an_ephemeral_port() {
        let app = TestApp::new();
        let (server, _dir) = start(&app);

        assert_eq!(
            server.address().ip(),
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            "the IPC server is not bound to loopback"
        );
        assert!(server.address().ip().is_loopback());
        assert_ne!(server.port(), 0, "an ephemeral bind must resolve to a real port");
    }

    /// A wrong token is refused, counted and logged. Logged because a refusal nobody records
    /// is a security event nobody can investigate — and the claimed agent id is recorded as a
    /// claim, since a stranger chose it.
    #[test]
    fn a_request_with_the_wrong_token_is_refused_and_logged() {
        let app = TestApp::new();
        let (server, _dir) = start(&app);
        let mut client = Client::connect(server.address());

        let response = client.raw(
            r#"{"token":"not-the-token","agent":"worker","request":{"verb":"send","to":"boss","text":"hi"}}"#,
        );
        assert!(!response.ok);
        assert_eq!(response.code, Some(ErrorCode::BadToken));
        assert_eq!(server.refusals(), 1);
        assert!(app.sent.lock().unwrap().is_empty(), "a bad token still ran the verb");

        let logged = app.refusals.lock().unwrap().clone();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].contains("send"), "{logged:?}");
        assert!(
            !logged[0].contains(server.token()),
            "the refusal log printed the session token"
        );

        // The right token, same connection, and it works — so the refusal was about the
        // credential and not about the connection being poisoned.
        let good = format!(
            r#"{{"token":"{}","agent":"worker","request":{{"verb":"send","to":"boss","text":"hi"}}}}"#,
            server.token()
        );
        assert!(client.raw(&good).ok);
        assert_eq!(app.sent.lock().unwrap().len(), 1);
    }

    /// The capability boundary, at the boundary. A worker asking to reconfigure a node is
    /// refused **before the handler is called** — which is the difference between a boundary
    /// and a convention, and the assertion is that `write_config` recorded nothing.
    #[test]
    fn only_a_meta_agent_may_reconfigure_another_node() {
        let app = TestApp::new();
        let (server, _dir) = start(&app);
        let mut client = Client::connect(server.address());

        let configure = |who: &str| {
            format!(
                r#"{{"token":"{}","agent":"{who}","request":{{"verb":"configure","node":"worker","model":{{"role_kind":"worker"}}}}}}"#,
                server.token()
            )
        };

        for role in ["worker", "boss"] {
            let response = client.raw(&configure(role));
            assert!(!response.ok, "{role} was allowed to reconfigure another node");
            assert_eq!(response.code, Some(ErrorCode::Refused));
            assert!(response.message().contains("meta"), "{}", response.message());
        }
        assert!(
            app.configured.lock().unwrap().is_empty(),
            "a refused configure still reached the handler"
        );

        let allowed = client.raw(&configure("meta"));
        assert!(allowed.ok, "the meta agent was refused: {}", allowed.message());
        let configured = app.configured.lock().unwrap().clone();
        assert_eq!(configured, ["worker"]);

        // And reading is the same capability: a worker may not read another node's
        // configuration either, since knowing it is the first half of rewriting it.
        let read = format!(
            r#"{{"token":"{}","agent":"worker","request":{{"verb":"configure","node":"meta"}}}}"#,
            server.token()
        );
        assert!(!client.raw(&read).ok);

        let read = format!(
            r#"{{"token":"{}","agent":"meta","request":{{"verb":"configure","node":"worker"}}}}"#,
            server.token()
        );
        let response = client.raw(&read);
        assert!(response.ok, "{}", response.message());
        assert!(matches!(response.answer, Some(Answer::Config { .. })));
    }

    /// A worker may not spawn. Territory and the cap are `orchestrator.rs`'s; *whether this
    /// role may spawn at all* is a capability and belongs at the same boundary `configure`
    /// does.
    #[test]
    fn only_a_managing_role_may_spawn() {
        let app = TestApp::new();
        let (server, _dir) = start(&app);
        let mut client = Client::connect(server.address());

        let spawn = |who: &str| {
            format!(
                r#"{{"token":"{}","agent":"{who}","request":{{"verb":"spawn","label":"Helper","role":"worker"}}}}"#,
                server.token()
            )
        };

        let refused = client.raw(&spawn("worker"));
        assert!(!refused.ok);
        assert_eq!(refused.code, Some(ErrorCode::Refused));

        // The orchestrator gets past the boundary and lands on the unwired default, which
        // refuses **by name** rather than silently succeeding.
        let allowed = client.raw(&spawn("boss"));
        assert!(!allowed.ok);
        assert!(allowed.message().contains("spawn"), "{}", allowed.message());
        assert!(allowed.message().contains("not wired up"), "{}", allowed.message());
    }

    /// Malformed input is **answered**, and the connection survives it. Closing the socket
    /// would look to an agent exactly like Velm crashing, which is the wrong thing to tell
    /// something that is about to retry — so the assertion is that a good request works on
    /// the *same* connection afterwards.
    #[test]
    fn malformed_json_answers_with_an_error_and_the_connection_survives() {
        let app = TestApp::new();
        let (server, _dir) = start(&app);
        let mut client = Client::connect(server.address());

        for rubbish in [
            "not json at all",
            "{",
            r#"{"token":"x"}"#,
            r#"{"token":"x","agent":"worker","request":{"verb":"teleport"}}"#,
            "[]",
        ] {
            let response = client.raw(rubbish);
            assert!(!response.ok, "{rubbish:?} was accepted");
            assert_eq!(response.code, Some(ErrorCode::Malformed), "{rubbish:?}");
        }

        let good = format!(
            r#"{{"token":"{}","agent":"worker","request":{{"verb":"note.read","path":"plan.md"}}}}"#,
            server.token()
        );
        let response = client.raw(&good);
        assert!(response.ok, "the connection did not survive five malformed lines");
        assert_eq!(response.answer, Some(Answer::Note { text: "# plan.md\n".into() }));
    }

    /// An id that names nothing is refused once, at the boundary, rather than in eight
    /// handlers — and the refusal says which id, because *"unknown agent"* on its own is a
    /// message nobody can act on.
    #[test]
    fn an_unknown_agent_is_refused_before_the_verb_runs() {
        let app = TestApp::new();
        let (server, _dir) = start(&app);
        let mut client = Client::connect(server.address());

        let line = format!(
            r#"{{"token":"{}","agent":"ghost","request":{{"verb":"send","to":"boss","text":"hi"}}}}"#,
            server.token()
        );
        let response = client.raw(&line);
        assert_eq!(response.code, Some(ErrorCode::UnknownAgent));
        assert!(response.message().contains("ghost"), "{}", response.message());
        assert!(app.sent.lock().unwrap().is_empty());
    }

    /// The runtime file is the credential on disk: private, and new every launch. A token
    /// that repeated across launches would still be valid after a crash, in a file somebody
    /// else's process may have read.
    #[test]
    fn the_runtime_file_is_private_and_its_token_is_new_every_launch() {
        let app = TestApp::new();
        let (first, dir) = start(&app);

        let path = RuntimeFile::path_in(dir.path());
        assert_eq!(first.runtime_path(), path);
        let written = RuntimeFile::read(&path).expect("no runtime file");
        assert_eq!(written.port, first.port());
        assert_eq!(written.token, first.token());
        assert_eq!(written.pid, std::process::id());
        assert_eq!(written.address().ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the runtime file was readable by other users: {mode:o}");
        }

        let (second, _other) = start(&app);
        assert_ne!(
            second.token(),
            first.token(),
            "two launches produced the same token — the source is not random"
        );
        assert_eq!(second.token().len(), 64, "a 32-byte token is 64 hex characters");
        assert!(second.token().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Stopping tidies up after itself — but only its own file. A newer server may have
    /// written over the top, and deleting that would leave a live server no shim could find.
    #[test]
    fn stopping_removes_only_its_own_runtime_file() {
        let app = TestApp::new();
        let dir = tempfile::tempdir().unwrap();
        let path = RuntimeFile::path_in(dir.path());

        let first_handler: Arc<dyn IpcHandler> = app.clone();
        let second_handler: Arc<dyn IpcHandler> = app.clone();
        let mut first = IpcServer::start(dir.path(), first_handler).unwrap();
        let second = IpcServer::start(dir.path(), second_handler).unwrap();

        // The second server owns the file now.
        assert_eq!(RuntimeFile::read(&path).unwrap().token, second.token());

        first.stop();
        let survivor = RuntimeFile::read(&path).expect("the older server deleted the newer file");
        assert_eq!(survivor.token, second.token());

        drop(second);
        assert!(RuntimeFile::read(&path).is_err(), "the last server left its file behind");
    }

    /// The whole loop through the code the shim actually runs: a runtime file, a real socket,
    /// a real image. Bytes in, bytes out — a base64 mistake would arrive as different bytes
    /// rather than as an error, which is why this compares the payload rather than the status.
    #[test]
    fn an_image_arrives_as_the_bytes_that_were_sent() {
        let app = TestApp::new();
        // Bound but not read: the server has to outlive the call, and dropping it here would
        // close the socket the runtime file is about to point at.
        let (_server, dir) = start(&app);

        let runtime = RuntimeFile::read(&RuntimeFile::path_in(dir.path())).unwrap();
        // A PNG header plus every byte value, so a sign error or a lost tail shows up.
        let mut payload = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        payload.extend((0u8..=255).rev());

        let response = runtime
            .call(
                "worker",
                Request::Image {
                    data: encode_base64(&payload),
                    caption: Some("a chart".into()),
                },
            )
            .expect("the call failed");
        assert!(response.ok, "{}", response.message());

        let received = app.images.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0], payload, "the image did not survive the wire");
        drop(received);

        // And rubbish in the data field is refused rather than handed to the blob store.
        let bad = runtime
            .call("worker", Request::Image { data: "!!!!".into(), caption: None })
            .unwrap();
        assert!(!bad.ok);
        assert_eq!(bad.code, Some(ErrorCode::Malformed));
        assert_eq!(app.images.lock().unwrap().len(), 1);
    }

    /// RFC 4648's own vectors, because "it round trips" is satisfied by any pair of functions
    /// that agree with each other and with nothing else — including a wrong alphabet.
    #[test]
    fn base64_matches_rfc_4648_and_survives_a_round_trip() {
        // RFC 4648's first vector is `("", "")` and it is **encode-only** here: the decoder
        // refuses the empty string on purpose (see its own doc comment), because the only
        // thing this pair carries is an image an agent posted and a zero-byte picture is the
        // defect the `"a"` refusal exists to stop, arriving by the honest route. The encoder
        // is still held to the vector.
        assert_eq!(encode_base64(b""), "");
        assert_eq!(decode_base64(""), None, "an empty payload was decoded as a zero-byte blob");

        let vectors = [
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(encode_base64(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(
                decode_base64(encoded).as_deref(),
                Some(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }

        // Every length through two full quantums, so no padding case is missed. From **one**,
        // because zero is the encode-only vector above.
        for length in 1..=32usize {
            let bytes: Vec<u8> = (0..length).map(|i| (i * 7 + 3) as u8).collect();
            let text = encode_base64(&bytes);
            assert_eq!(text.len() % 4, 0, "padding is wrong at length {length}");
            assert_eq!(decode_base64(&text).as_deref(), Some(bytes.as_slice()));
        }

        // The high bytes exercise the two alphabet characters a naive table gets wrong.
        assert_eq!(encode_base64(&[0xfb, 0xff, 0xbf]), "+/+/");
        assert_eq!(decode_base64("+/+/"), Some(vec![0xfb, 0xff, 0xbf]));

        // Every refusal below is a **near miss** — the shape a mistake takes, not the shape
        // rubbish takes. That is the point: a decoder is only worth anything on the inputs
        // that nearly work.
        //
        // ⚠ An earlier version of this test asserted `decode_base64("hello world") == None`
        // and the decoder correctly returned seven bytes: every character of "helloworld" is
        // in the alphabet and the space was being skipped. The assertion was wrong *and* it
        // named a real weakness — the decoder was lenient about whitespace and length — so
        // both were fixed. `"hello world"` is refused now, for its length rather than for
        // being prose, which is the only thing a decoder can actually know.
        assert_eq!(decode_base64("hello world"), None, "11 characters is not a base64 length");
        // Eight characters each, so these are refused for the character and not for the
        // length — a vector that fails the first check never exercises the second.
        assert_eq!(decode_base64("Zm9vYm!y"), None, "a character outside the alphabet");
        assert_eq!(decode_base64("bad*data"), None);
        assert_eq!(decode_base64("und_scor"), None);

        // A truncated payload: valid characters, impossible length.
        assert_eq!(decode_base64("Zm9vY"), None);
        assert_eq!(decode_base64("Zm9vYm"), None, "the unpadded form is refused too");

        // Two payloads spliced together put a `=` in the middle.
        assert_eq!(decode_base64("Zg==Zg=="), None);
        assert_eq!(decode_base64("Z=g="), None);
        assert_eq!(decode_base64("===="), None);

        // Whitespace is never data. Every producer of this wire format is `encode_base64`,
        // which emits none, so a newline in a data field means the framing went wrong — and
        // this is a line-delimited protocol, where that is exactly what must not be shrugged
        // off.
        assert_eq!(decode_base64("Zm9v\nYmFy"), None);
        assert_eq!(decode_base64("Zm9v YmFy"), None);

        // Non-canonical trailing bits. `Zg==` and `Zh==` differ by bits the decoder would
        // otherwise discard, so a lenient decoder gives one payload two spellings — and a
        // content-addressed blob store gives one picture two hashes.
        assert_eq!(decode_base64("Zg=="), Some(b"f".to_vec()));
        assert_eq!(decode_base64("Zh=="), None, "a non-canonical tail decoded as if it were f");
        assert_eq!(decode_base64("Zm9vYmE="), Some(b"fooba".to_vec()));
        assert_eq!(decode_base64("Zm9vYmF="), None, "a non-canonical tail decoded as fooba");
    }

    /// The two structs that carry the IPC token must not print it, and both used to.
    ///
    /// Latent when it was found — nothing formats either one — which is exactly the argument
    /// a derive wins on and exactly why it is wrong: the moment somebody debugs a refused
    /// verb, the credential that authorises *every* verb goes into the output. This crate
    /// hand-writes redacting impls for `Credentials`, `ClaudeCli` and `AcpTransport` already,
    /// so these two were the odd ones out rather than the precedent.
    #[test]
    fn neither_the_runtime_file_nor_an_envelope_prints_its_token() {
        let secret = "tok-3f9a-never-print-me";

        let file = RuntimeFile { port: 51234, token: secret.to_owned(), pid: 4242 };
        let printed = format!("{file:?}");
        assert!(!printed.contains(secret), "the runtime file printed its token: {printed}");
        assert!(!printed.contains("token"), "even the field name invites a second look");
        // The diagnostic half is kept: a stale file is diagnosed by its port and its pid.
        assert!(printed.contains("51234") && printed.contains("4242"), "{printed}");

        let envelope = Envelope {
            token: secret.to_owned(),
            agent: "node-7".to_owned(),
            request: Request::Send { to: "node-8".to_owned(), text: "hello".to_owned() },
        };
        let printed = format!("{envelope:?}");
        assert!(!printed.contains(secret), "the envelope printed its token: {printed}");
        // The verb and the caller are the useful half of a wire dump and neither is a secret.
        assert!(printed.contains("node-7") && printed.contains("node-8"), "{printed}");

        // ⚠ And through a container, which is how it would actually happen: a derived `Debug`
        // on any struct holding one of these prints it with *this* impl.
        let nested = format!("{:?}", vec![file]);
        assert!(!nested.contains(secret), "the token escaped inside a container: {nested}");
    }

    /// A token is compared without an early exit, and — much more importantly — a wrong
    /// length never indexes past the end of the shorter string.
    #[test]
    fn secrets_are_compared_whole() {
        assert!(secrets_match("abc", "abc"));
        assert!(!secrets_match("abc", "abd"));
        assert!(!secrets_match("", "abc"));
        assert!(!secrets_match("abcd", "abc"));
        assert!(secrets_match("", ""));
    }
}
