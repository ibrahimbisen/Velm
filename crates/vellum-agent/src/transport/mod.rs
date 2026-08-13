//! The four ways Velm runs an agent, behind one interface.
//!
//! `docs/07-agent-canvas.md` §5 is the contract: **every transport produces the same
//! [`TranscriptEvent`] stream**, so the painter, the two display modes, the sidecar and the
//! away-mode digest know nothing about which kind of process is running. Adding a transport is
//! a new producer and no new consumer — which is exactly what [`claude_cli`] was.
//!
//! ```text
//!   LaunchSpec ──► transport::start ─┬─ claude_cli  the `claude` CLI's own stream-json
//!        │                           ├─ acp         a child speaking JSON-RPC over stdio
//!        │                           ├─ pty         a child in a real pseudo-terminal
//!        │                           └─ http        an API, including one on this machine
//!        │                                               │
//!        └──────────► Sender<TranscriptEvent> ◄──────────┘
//! ```
//!
//! ⚠ **[`claude_cli`] supersedes [`acp`] for Claude, on measured evidence.** `claude` does not
//! speak the Agent Client Protocol: it speaks a line-delimited JSON protocol of its own, and
//! the capture that establishes that is quoted in `claude_cli`'s own tests. `acp` remains for
//! agents that genuinely speak ACP; whether `codex` and `gemini` do is **unverified** — those
//! two have never been run from here.
//!
//! # No async, and why that is not a limitation
//!
//! Every transport is blocking work on a worker thread that posts answers back through a
//! `std::sync::mpsc` channel — the shape `vellum-app`'s `links.rs` already uses for link
//! fetches and `vellum-store` uses for autosave. A `tokio` runtime would be a second
//! execution model to reason about in exchange for concurrency a handful of agent turns
//! does not need, and `docs/07` §5c rules it out by name. The consequence to respect: a
//! transport method **must never block the caller** — anything that waits happens on the
//! transport's own thread.
//!
//! # The trait is `AgentTransport`, not `Transport`
//!
//! [`crate::provider::Transport`] is an enum — *which* of the three a node chose — and it is
//! re-exported at the crate root as `vellum_agent::Transport`. Two items of that name in one
//! crate would mean qualifying one of them at every use site, so the trait takes the longer
//! name: it appears in a handful of signatures, while the enum is stored on every node.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::provider::{ProviderChoice, Transport as TransportKind};
use crate::transcript::{RequestId, TranscriptEvent, TurnId};
use crate::{AgentError, Result};

pub mod acp;
pub mod claude_cli;
pub mod http;
pub mod pty;

/// Everything needed to start one agent, resolved by the caller.
///
/// Deliberately flat and owned: it crosses a thread boundary into a worker and must not
/// borrow anything from the document. Note what is **already resolved** by the time it gets
/// here — `system_context` is the finished string (role label + the rule cascade + context
/// sources), because `vellum-agent` does not read the board and `crate::rules` is where that
/// resolution lives.
/// **Never logged.** The `Debug` impl below says *whether* there is a key and never what it
/// is; see it for why a derive on this particular struct is the worst of the four.
#[derive(Clone, Default)]
pub struct LaunchSpec {
    /// Which provider, which model, which transport. See [`ProviderChoice`].
    pub provider: ProviderChoice,

    /// The binary to run, overriding [`crate::provider::Provider::default_command`].
    ///
    /// **A default is a default, not a fact.** `claude`, `codex` and `gemini` ship on their
    /// own schedules and rename their protocol entry points; the session probes for the
    /// binary before trusting either value ([`probe_command`]), so a wrong guess costs a
    /// config field and never a broken feature.
    pub command: Option<String>,

    /// Arguments appended after any the transport itself requires.
    pub args: Vec<String>,

    /// The agent's working directory. `None` inherits Velm's, which is almost never what a
    /// coding agent wants — the app passes the node's `working_dir` or the project root.
    pub cwd: Option<PathBuf>,

    /// Extra environment. This is how `VELM_IPC` and `VELM_AGENT_ID` reach the child so the
    /// `velm-agent-cli` shim can talk back (`docs/07` §6).
    pub env: Vec<(String, String)>,

    /// The resolved system context: role label, rules, context sources. Passed to the model
    /// as a system prompt (HTTP) or as the session's instructions (ACP).
    pub system_context: String,

    /// Overrides [`crate::provider::Provider::default_base_url`]. Required for
    /// [`crate::provider::Provider::Local`] and [`crate::provider::Provider::Custom`], which *are* their endpoint.
    pub base_url: Option<String>,

    /// An API key supplied by the caller. `None` falls back to the credentials file and then
    /// to the environment — see [`http::Credentials`]. **Never logged, never put in an
    /// event.**
    pub api_key: Option<String>,

    /// Velm's data directory, used only to find `credentials.json`. `None` skips the file
    /// and uses the environment.
    pub data_dir: Option<PathBuf>,

    /// The pseudo-terminal's size in (columns, rows). `None` takes [`pty::DEFAULT_SIZE`].
    /// Ignored by the other two transports.
    pub terminal: Option<(u16, u16)>,

    /// Where Velm's own agent-facing binaries are, if they were found. See [`Shim`].
    ///
    /// **Data on the spec rather than a global lookup inside each transport**, for the same
    /// reason `system_context` is: a transport that resolved this for itself could not be
    /// handed a fake one by a test, and the whole of `docs/07` §6 — messaging, notes, spawn,
    /// options — is reachable only if this is right. `None` means the shim was not found and
    /// the agent runs with no way to act on the board; it is a degraded session, not a
    /// failed one.
    pub shim: Option<Shim>,
}

/// ⚠ **Hand-written, and this is the most exposed of the crate's four secrets.**
///
/// A `LaunchSpec` is what every transport's `start` is handed, so it is the argument in every
/// stack frame where starting an agent goes wrong — and *"the agent would not start"* is the
/// commonest failure this crate has, which makes it the struct somebody is most likely to
/// `dbg!`. `#[derive(Debug)]` would put the user's API key in that output, and it would put it
/// there on the one path where the output is copied into a bug report.
///
/// `env` is redacted for the same reason and it is not belt and braces: it is how `VELM_IPC`
/// and `VELM_AGENT_ID` reach the child, and `VELM_IPC_TOKEN` is a documented way to pass the
/// IPC credential — so the map holds a second secret whenever that form is used. The *names*
/// are printed, which is what a "was the environment wired up" question needs.
impl std::fmt::Debug for LaunchSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("provider", &self.provider)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &self.env.iter().map(|(name, _)| name).collect::<Vec<_>>())
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("data_dir", &self.data_dir)
            .field("terminal", &self.terminal)
            // Paths to our own binaries — no secret, and *"was the shim found"* is the first
            // question when an agent cannot reach the board.
            .field("shim", &self.shim)
            .finish_non_exhaustive()
    }
}

impl LaunchSpec {
    pub fn new(provider: ProviderChoice) -> Self {
        Self { provider, ..Self::default() }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_system_context(mut self, context: impl Into<String>) -> Self {
        self.system_context = context.into();
        self
    }

    /// The command this spec will actually run, resolving `None` through the provider.
    ///
    /// `None` means *this provider has no CLI to delegate to* — Kimi, a local server — which
    /// is a different statement from "the binary is missing".
    pub fn resolved_command(&self) -> Option<&str> {
        self.command.as_deref().or_else(|| self.provider.provider.default_command())
    }

    pub fn with_shim(mut self, shim: Shim) -> Self {
        self.shim = Some(shim);
        self
    }

    /// The API base, resolving `None` through the node's own choice and then the provider.
    ///
    /// **Three layers, and the middle one is new.** [`crate::provider::Provider::Local`] and
    /// [`crate::provider::Provider::Custom`] deliberately have no default — they *are* their
    /// endpoint — so before [`ProviderChoice::base_url`] existed there was no way to say
    /// where a local model was listening except by setting this field, which nothing in the
    /// application did. The order is caller override, then the node, then the provider's
    /// fixed default; an empty string is treated as unset, because that is what a config
    /// field the user cleared answers.
    pub fn resolved_base_url(&self) -> Option<&str> {
        self.base_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .or_else(|| self.provider.base_url.as_deref().filter(|url| !url.trim().is_empty()))
            .or_else(|| self.provider.provider.default_base_url())
    }

    /// A variable this spec will put in the child's environment.
    ///
    /// The IPC coordinates travel in [`LaunchSpec::env`] because that is how they reach the
    /// child, and an MCP server entry has to repeat them in its own `env` block — so the two
    /// must be read from one place or they will come to disagree about which board an agent
    /// is on.
    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// The MCP server document to hand a client that speaks MCP, as `{"mcpServers": {…}}`.
    ///
    /// `None` when there is no `velm-mcp` to point at, which is the degradation: the agent
    /// is started with no MCP configuration at all rather than with one naming a binary that
    /// is not there.
    pub fn mcp_config(&self) -> Option<serde_json::Value> {
        let shim = self.shim.as_ref()?;
        // Built rather than written as a `json!` literal because the key is a constant:
        // `MCP_SERVER_NAME` is part of every tool name the agent sees, so it is spelled once
        // and reused, and a macro key has to be a literal.
        let mut servers = serde_json::Map::new();
        servers.insert(MCP_SERVER_NAME.to_owned(), shim.mcp_entry(self)?);
        Some(serde_json::json!({ "mcpServers": serde_json::Value::Object(servers) }))
    }

    /// The same servers in the shape the Agent Client Protocol asks for: an **array** of
    /// entries that name themselves, with `env` as a list of `{name, value}` pairs.
    ///
    /// Empty when there is nothing to register, which is exactly the `[]` this used to be
    /// hardcoded to — so a client that would have worked before still works.
    pub fn mcp_servers_acp(&self) -> Vec<serde_json::Value> {
        let Some(shim) = self.shim.as_ref() else {
            return Vec::new();
        };
        let Some(mcp) = shim.mcp() else {
            return Vec::new();
        };
        vec![serde_json::json!({
            "name": MCP_SERVER_NAME,
            "command": mcp.display().to_string(),
            "args": [],
            "env": self.ipc_env().into_iter().map(|(name, value)| {
                serde_json::json!({ "name": name, "value": value })
            }).collect::<Vec<_>>(),
        })]
    }

    /// The two variables that tell a child which Velm and which node it belongs to.
    fn ipc_env(&self) -> Vec<(&'static str, String)> {
        [crate::mcp::IPC_ENV, crate::mcp::AGENT_ID_ENV]
            .into_iter()
            .filter_map(|name| self.env_value(name).map(|value| (name, value.to_owned())))
            .collect()
    }
}

/// A picture an agent produced, on its way to the blob store.
///
/// **This crate must not write to the blob store**, which lives in `vellum-store` and is
/// reached through `vellum-app` — so a transport that receives image bytes parks them here
/// and emits `TranscriptEvent::Image { blob }` carrying [`PendingBlob::id`] as a
/// *placeholder*. The app drains the blobs, writes each one, and substitutes the real BLAKE3
/// hash for the placeholder.
///
/// ⚠ **Drain blobs before events, and substitute before appending to the sidecar.** The
/// worker pushes the blob *then* sends the event, so a caller that reads events first can
/// see a placeholder it has no bytes for; and a transcript written with the placeholder still
/// in it reads back, after a restart, as a picture that resolves to nothing.
#[derive(Debug, Clone)]
pub struct PendingBlob {
    /// The placeholder carried by the matching `TranscriptEvent::Image`. Unique per session.
    pub id: String,
    /// `image/png`, `image/jpeg`… as the agent declared it.
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// The queue a transport parks [`PendingBlob`]s in. Shared with its worker threads.
pub(crate) type Blobs = Arc<Mutex<Vec<PendingBlob>>>;

/// Parks bytes and answers the placeholder to put in the event.
///
/// The placeholder is deliberately **not** a hash: hashing here would duplicate the blob
/// store's own addressing in a crate that must not depend on it, and would be wrong the
/// moment the store changes algorithm.
///
/// ⚠ **The counter is process-wide, not per queue.** Numbering from the queue's length reads
/// correctly and is wrong: `take_pending_blobs` drains it, so the next turn starts at
/// `pending:0` again — and an app that keeps its placeholder→hash map across frames would
/// resolve this turn's picture to the last one's. A test that only checks two blobs in one
/// queue cannot see it.
pub(crate) fn park_blob(blobs: &Blobs, mime: &str, bytes: Vec<u8>) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!(
        "pending:{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    blobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(PendingBlob { id: id.clone(), mime: mime.to_owned(), bytes });
    id
}

/// Base64, for the image bytes an agent sends inline — **[`crate::ipc`]'s strict decoder**,
/// re-exported so the three call sites on the image path share one definition.
///
/// # There used to be three of these, and two of them were lenient
///
/// This module had its own and [`acp`] had another, both skipping `=` and whitespace anywhere,
/// both accepting a length that is not a multiple of four, and both ignoring non-canonical
/// trailing bits — while each claimed in its own doc comment to answer `None` on anything that
/// is not base64. `decode_base64("a")` answered `Some(vec![])`: a **zero-byte blob**, parked
/// by [`park_blob`] and written to the content-addressed store as though it were a picture.
///
/// `ipc::decode_base64` was already the strict one, and already carried the argument for why
/// (trap 2 in `CLAUDE.md`, at a different layer: something that always decodes is a decoder
/// that never reports a bug). Two spellings of one rule is how they came to disagree, so there
/// is one now.
///
/// ⚠ **Behaviour change, deliberately.** Line-wrapped base64 (`"aGVs\nbG8="`), an unpadded
/// length (`"aGVsbG8"`) and a `=` in the middle are all refused now where they used to decode.
/// Neither protocol on this path emits any of those — ACP's flat `data`/`mimeType` and
/// Anthropic's `source.data`/`source.media_type` are both single unwrapped tokens — and each
/// of those shapes is what a *truncated or spliced* payload looks like, which is exactly the
/// thing worth refusing on the way into a blob store.
///
/// ⚠ **And the empty string, which unifying the three did not catch.** `""` is legitimate
/// base64 for zero bytes, so it kept decoding to the very `vec![]` the `"a"` refusal exists to
/// stop — and both remaining test files asserted that was correct, two lines from the
/// assertion that says the opposite. It is refused at the decoder rather than at the three
/// callers, because none of them checks `is_empty()` and a rule applied at two of three sites
/// is the shape of the defect rather than the fix for it.
pub(crate) use crate::ipc::decode_base64;

/// One running agent, whatever kind of process is behind it.
///
/// Implementors are owned by [`crate::session::Session`], which is what serialises the calls:
/// a prompt arriving mid-turn is queued there rather than interleaved here.
pub trait AgentTransport: Send {
    /// Which kind this is. For the node's header — *"this one runs on your subscription"* —
    /// without the caller matching on a concrete type.
    fn kind(&self) -> TransportKind;

    /// Starts a turn. **Returns immediately**; the answer arrives as events.
    ///
    /// The turn id is allocated by the session, not here, because it is the key the
    /// transcript on disk is read back by and a transport is restarted more often than a
    /// node is. The transport emits `TurnStarted` and, eventually, exactly one `TurnEnded`
    /// carrying the same id.
    ///
    /// ⚠ **An `Err` means no `TurnStarted` was emitted.** The two events are a paired
    /// begin/end and the caller cannot close a pair it never opened:
    /// [`crate::session::Session`] clears its own optimistic state on the `Err`, but an event
    /// already in the channel is absorbed on the next `poll`, sets `Status::Running`, and is
    /// never followed by a `TurnEnded` — so every later prompt is refused for the life of the
    /// session. This is trap 11's shape (a `?` on the unwind path of a paired begin/end), so
    /// an implementation that spawns a worker must emit the start **from the worker**, where
    /// the pairing is structural, rather than before a fallible spawn.
    fn send_prompt(&mut self, turn: TurnId, prompt: &str) -> Result<()>;

    /// Stops the turn in flight. A no-op when none is.
    ///
    /// Best-effort by nature: an HTTP request already in the provider's hands cannot be
    /// unsent, and an ACP agent may finish the tool call it is inside. What is guaranteed is
    /// that the turn ends with `TurnOutcome::Cancelled` and no further output is attributed
    /// to it.
    fn cancel(&mut self) -> Result<()>;

    /// Answers a `TranscriptEvent::PermissionRequest`.
    ///
    /// The transport emits the matching `PermissionAnswer` itself rather than leaving the
    /// caller to, so the answer lands *in the one ordered stream* the sidecar records — a
    /// transcript where the answer is missing or out of order cannot be replayed.
    ///
    /// The default is a no-op: a transport that never asks has nothing to answer.
    fn answer_permission(&mut self, id: &RequestId, allowed: bool) -> Result<()> {
        let _ = (id, allowed);
        Ok(())
    }

    /// Types into the agent's session.
    ///
    /// The PTY's whole point (`docs/07` §5b): one agent can type into another's terminal.
    /// Refused by the transports that have no terminal, rather than silently doing nothing.
    fn write_input(&mut self, text: &str) -> Result<()> {
        let _ = text;
        Err(AgentError::Refused(
            "this agent has no terminal to type into — only a terminal session accepts input"
                .into(),
        ))
    }

    /// Takes the images this transport has received since the last call. See [`PendingBlob`].
    fn take_pending_blobs(&mut self) -> Vec<PendingBlob> {
        Vec::new()
    }

    /// Whether the process behind this transport is still there.
    ///
    /// `true` for HTTP, which has no process to lose. For ACP and PTY it is what lets the
    /// session say *"the agent exited"* without fabricating an event for something that is
    /// not a turn.
    fn is_alive(&self) -> bool {
        true
    }

    /// Ends the session and releases the process.
    ///
    /// Must be idempotent — the session calls it on drop as well as on request — and must
    /// answer any outstanding permission request as cancelled, or the child waits forever
    /// for a reply that is never coming.
    fn shutdown(&mut self) -> Result<()>;
}

/// Starts the transport this spec asks for.
///
/// The one place the three are chosen between, so a node's `ProviderChoice` is the only
/// thing that decides — there is no second opinion in `vellum-app`.
pub fn start(spec: &LaunchSpec, events: Sender<TranscriptEvent>) -> Result<Box<dyn AgentTransport>> {
    match spec.provider.effective_transport() {
        TransportKind::ClaudeCli => Ok(Box::new(claude_cli::ClaudeCli::start(spec, events)?)),
        TransportKind::Acp => Ok(Box::new(acp::AcpTransport::start(spec, events)?)),
        TransportKind::Pty => Ok(Box::new(pty::PtyTransport::start(spec, events)?)),
        TransportKind::Http => Ok(Box::new(http::HttpTransport::start(spec, events)?)),
    }
}

/// Finds an executable, the way a shell would.
///
/// Run **before** spawning, on purpose: a spawn failure answers `NotFound` with no useful
/// context on some platforms and a confusing one on others, and *"`claude` is not installed
/// or is not on your PATH"* is the single most common thing this crate has to say. Absolute
/// and relative paths are checked directly; a bare name is looked up on `PATH`.
pub fn probe_command(command: &str) -> Result<PathBuf> {
    if command.is_empty() {
        return Err(AgentError::MissingCommand { command: String::new() });
    }

    let named = Path::new(command);
    if named.components().count() > 1 {
        return if is_executable(named) {
            Ok(named.to_path_buf())
        } else {
            Err(AgentError::MissingCommand { command: command.to_owned() })
        };
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(command);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
        // Windows spells the extension separately from the name, and the shell tries the
        // ones in `PATHEXT` in order — `claude` there is `claude.cmd` in practice, so a
        // lookup without this finds nothing on a machine where the tool is installed.
        #[cfg(windows)]
        {
            let extensions =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned());
            for extension in extensions.split(';').filter(|part| !part.is_empty()) {
                let suffixed = directory.join(format!("{command}{extension}"));
                if is_executable(&suffixed) {
                    return Ok(suffixed);
                }
            }
        }
    }
    Err(AgentError::MissingCommand { command: command.to_owned() })
}

// ---------------------------------------------------------------------------------------
// The shim — how an agent reaches back into Velm
// ---------------------------------------------------------------------------------------

/// The shim an agent shells out to. `crates/vellum-agent/src/bin/velm_agent_cli.rs`.
///
/// Spelled here rather than repeated at the four sites that need it, because it is
/// simultaneously a `[[bin]]` name in `Cargo.toml`, a file the packaging scripts copy, a
/// word in the system context an agent reads, and the thing this module probes for. Those
/// four agreeing is the entire feature.
pub const AGENT_CLI: &str = "velm-agent-cli";

/// Velm's MCP stdio server. `crates/vellum-agent/src/bin/velm_mcp.rs`.
pub const MCP_SERVER: &str = "velm-mcp";

/// The name an MCP client files Velm's server under.
///
/// It is not cosmetic: Claude Code exposes an MCP tool as `mcp__<server>__<tool>`, so this
/// word is part of every tool name the agent sees. Short and lowercase for that reason.
pub const MCP_SERVER_NAME: &str = "velm";

/// Where Velm's own agent-facing binaries are.
///
/// # Why `current_exe`'s directory, and why it is probed rather than assumed
///
/// The shim has to be found from inside a running Velm, and the two places Velm ever runs
/// from put it in the same relation: in a `.app` bundle both binaries sit in
/// `Contents/MacOS/` beside the main executable, and in a Cargo build they sit in
/// `target/<profile>/` beside it. So the answer is *"the directory I was loaded from"* in
/// both, and it is the only answer that does not encode a build layout into the program.
///
/// It is **probed** — [`Shim::beside`] stats each name — because a bundle built before this
/// existed, or one updated by a script that copies a single executable, has the main binary
/// and neither shim. That must produce [`Shim::gap`]'s named message rather than agents that
/// silently cannot reach the board, which is the state this whole change exists to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shim {
    dir: PathBuf,
    cli: Option<PathBuf>,
    mcp: Option<PathBuf>,
}

impl Shim {
    /// What is beside `dir`. **Pure**: it stats, and it reads no environment.
    pub fn beside(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let cli = executable_in(&dir, AGENT_CLI);
        let mcp = executable_in(&dir, MCP_SERVER);
        Self { dir, cli, mcp }
    }

    /// What is beside the running executable, or `None` if the platform will not say where
    /// that is. Not cached here — see [`shim`].
    pub fn locate() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        Some(Self::beside(exe.parent()?))
    }

    /// The directory the child gets on its `PATH`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cli(&self) -> Option<&Path> {
        self.cli.as_deref()
    }

    pub fn mcp(&self) -> Option<&Path> {
        self.mcp.as_deref()
    }

    /// Whether anything at all was found. `false` is the "nothing was shipped" case.
    pub fn any(&self) -> bool {
        self.cli.is_some() || self.mcp.is_some()
    }

    /// What is missing, in a sentence the user can act on — `None` when nothing is.
    ///
    /// It names the script that fixes it, because *"velm-agent-cli was not found"* is a
    /// sentence a user cannot do anything with and *"rebuild the bundle"* is one they can.
    pub fn gap(&self) -> Option<String> {
        let missing: Vec<&str> = [(AGENT_CLI, self.cli.is_some()), (MCP_SERVER, self.mcp.is_some())]
            .into_iter()
            .filter(|(_, found)| !found)
            .map(|(name, _)| name)
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "Velm's agent tools ({}) are not beside the application in {} — agents will run \
             but cannot message each other, read notes or spawn helpers. Rebuild with \
             scripts/make-app.sh.",
            missing.join(" and "),
            self.dir.display()
        ))
    }

    /// The `PATH` a child should get: this directory first, then whatever Velm inherited.
    ///
    /// **Prepended, never replaced.** A child that lost the inherited `PATH` would lose
    /// `git`, `node`, the user's toolchain and — for a delegated CLI — the very binary
    /// `probe_command` resolved, so a coding agent would come up unable to do the work it
    /// was placed on the board for.
    pub fn path_env(&self) -> (String, String) {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut directories = vec![self.dir.clone()];
        directories.extend(std::env::split_paths(&inherited).filter(|entry| entry != &self.dir));
        let joined = std::env::join_paths(directories)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.dir.display().to_string());
        ("PATH".to_owned(), joined)
    }

    /// One `mcpServers` entry, in the shape Claude Code's own configuration file uses.
    ///
    /// The `env` block repeats the IPC coordinates rather than trusting inheritance. It is
    /// belt and braces on the transports here, which do pass `env` through — and it is not
    /// belt and braces at all for a client that starts its MCP servers from a scrubbed
    /// environment, which several do.
    fn mcp_entry(&self, spec: &LaunchSpec) -> Option<serde_json::Value> {
        let mcp = self.mcp.as_ref()?;
        let env: serde_json::Map<String, serde_json::Value> = spec
            .ipc_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), serde_json::Value::String(value)))
            .collect();
        Some(serde_json::json!({
            "command": mcp.display().to_string(),
            "args": [],
            "env": env,
        }))
    }
}

/// The shim beside this process, resolved once.
///
/// Cached because [`Shim::beside`] stats two files and this is asked on every agent launch,
/// and because the answer cannot change while the process runs: the executable is open.
pub fn shim() -> Option<&'static Shim> {
    static FOUND: std::sync::OnceLock<Option<Shim>> = std::sync::OnceLock::new();
    FOUND.get_or_init(Shim::locate).as_ref()
}

/// `dir/name`, if that is something we could run. Tries the Windows spelling too.
fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let bare = dir.join(name);
    if is_executable(&bare) {
        return Some(bare);
    }
    // Not gated on `cfg(windows)`: a bundle assembled on one platform is occasionally
    // inspected on another, and looking for a name that cannot be there costs one `stat`.
    let suffixed = dir.join(format!("{name}.exe"));
    is_executable(&suffixed).then_some(suffixed)
}

/// Whether this path names a file we could run.
///
/// On Unix that is a file with any execute bit set; elsewhere the existence of the file is
/// all the filesystem will tell us, and claiming more would be a guess.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;

    /// A missing binary must be *named*, because naming it is the whole remedy. The empty
    /// case is here because `resolved_command` can answer `Some("")` from a config field a
    /// user cleared, and `PATH` lookup of `""` finds every directory.
    #[test]
    fn probing_names_the_command_it_could_not_find() {
        let error = probe_command("velm-definitely-not-installed").unwrap_err();
        assert!(
            error.to_string().contains("velm-definitely-not-installed"),
            "the error did not name the command: {error}"
        );
        assert!(matches!(error, AgentError::MissingCommand { .. }));

        assert!(matches!(probe_command("").unwrap_err(), AgentError::MissingCommand { .. }));
        // A path that exists but is a directory is not a command. Without the `is_file`
        // check this passes on every Unix, because directories carry the execute bit.
        assert!(probe_command("/tmp/").is_err());
    }

    /// Every machine that can run the test suite has a shell, so this is a real positive
    /// rather than a tautology — it proves the `PATH` walk finds something, which a test
    /// that only ever asserted failure would not.
    #[test]
    #[cfg(unix)]
    fn probing_finds_a_binary_that_is_really_there() {
        let found = probe_command("sh").expect("`sh` is on PATH on every Unix");
        assert!(found.is_absolute(), "{}", found.display());
        assert!(probe_command("/bin/sh").is_ok());
    }

    /// A picture that half decodes is a texture upload of garbage rather than a smaller
    /// picture, so anything that is not base64 must answer `None` rather than bytes.
    ///
    /// ⚠ **This test used to assert the bug.** Its middle line was
    /// `decode_base64("aGVs\nbG8=") == Some(b"hello")` under a comment reading *"padding and
    /// line breaks are both legal in a wire payload"* — a decoder that skips whitespace
    /// anywhere is one that cannot tell a wrapped payload from a spliced one, and the same
    /// leniency answered `Some(vec![])` for the single character `"a"`. A zero-byte blob went
    /// into the content-addressed store as a picture. The three copies of this function are
    /// one now, and it is the strict one.
    #[test]
    fn base64_decodes_a_picture_and_refuses_what_is_not_one() {
        assert_eq!(decode_base64("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64("aGVsbG8gd29ybGQ=").as_deref(), Some(&b"hello world"[..]));
        assert_eq!(decode_base64("not base64!"), None);

        // The near misses — the shape a *mistake* takes, rather than the shape rubbish takes.
        assert_eq!(decode_base64("a"), None, "one character decoded to a zero-byte blob");
        // ⚠ **And the same blob by the honest route.** This line used to read
        // `assert_eq!(decode_base64(""), Some(Vec::new()))`, two lines above the one that
        // refuses a zero-byte blob from `"a"` — so half of the defect was fixed and the other
        // half was *asserted to be correct*. No caller of this function checks `is_empty()`:
        // all three decode an image an agent posted and hand the bytes to the blob store.
        assert_eq!(decode_base64(""), None, "an empty payload is not a picture");
        assert_eq!(decode_base64("aGVsbG8"), None, "an unpadded length is a truncated payload");
        assert_eq!(decode_base64("aGVs\nbG8="), None, "whitespace inside the payload");
        assert_eq!(decode_base64("aGk=aGk="), None, "two payloads spliced together");
        assert_eq!(decode_base64("Zh=="), None, "a non-canonical tail is a second spelling");
    }

    /// A `LaunchSpec` is the argument in every frame where starting an agent went wrong,
    /// which makes it the struct somebody reaches for `dbg!` on — and it carries the user's
    /// API key. The derive that used to be here would have put that key in the output of the
    /// one debugging session most likely to end up pasted into a bug report.
    ///
    /// `env` is checked as well and it is not belt and braces: `VELM_IPC_TOKEN` is a
    /// documented way to pass the IPC credential to a child, so the map holds a second secret.
    #[test]
    fn a_launch_spec_debug_says_a_key_is_set_and_never_what_it_is() {
        let mut spec = LaunchSpec::new(ProviderChoice::new(Provider::Claude));
        spec.api_key = Some("sk-ant-notarealkey-0123456789".into());
        spec.env = vec![
            ("VELM_AGENT_ID".into(), "node-4".into()),
            ("VELM_IPC_TOKEN".into(), "a-token-nobody-should-print".into()),
        ];

        let printed = format!("{spec:?}");
        assert!(!printed.contains("sk-ant"), "the api key was printed: {printed}");
        assert!(!printed.contains("notarealkey"), "the api key was printed: {printed}");
        assert!(
            !printed.contains("a-token-nobody-should-print"),
            "an environment secret was printed: {printed}"
        );

        // Redacted, not omitted: "is a key set at all" is the actual question being debugged.
        assert!(printed.contains("api_key"), "{printed}");
        assert!(printed.contains("<set>"), "{printed}");
        assert!(printed.contains("VELM_IPC_TOKEN"), "the variable's name is not the secret");

        let bare = format!("{:?}", LaunchSpec::new(ProviderChoice::new(Provider::Claude)));
        assert!(bare.contains("None"), "an unset key must be visibly unset: {bare}");
    }

    /// A launch spec must not have to be told what a Claude node already implies, and must
    /// let the user override it — that split is what keeps a renamed CLI a config change.
    #[test]
    fn a_spec_falls_back_to_the_providers_defaults_and_yields_to_an_override() {
        let claude = LaunchSpec::new(ProviderChoice::new(Provider::Claude));
        assert_eq!(claude.resolved_command(), Some("claude"));
        assert_eq!(claude.resolved_base_url(), Some("https://api.anthropic.com"));

        let renamed = claude.clone().with_command("claude-code-acp");
        assert_eq!(renamed.resolved_command(), Some("claude-code-acp"));

        // A local model *is* its endpoint: there is nothing to guess and guessing
        // `localhost:11434` would silently talk to whichever server happened to be up.
        let local = LaunchSpec::new(ProviderChoice::new(Provider::Local));
        assert_eq!(local.resolved_command(), None);
        assert_eq!(local.resolved_base_url(), None);
    }

    /// The placeholder must be unique within a session and must not look like a hash, or a
    /// caller that forgot to substitute it would write a plausible-looking dead reference
    /// into a transcript.
    #[test]
    fn parked_blobs_get_distinct_placeholders_that_are_not_hashes() {
        let blobs: Blobs = Blobs::default();
        let first = park_blob(&blobs, "image/png", vec![1, 2, 3]);
        let second = park_blob(&blobs, "image/png", vec![4]);
        assert_ne!(first, second);
        assert!(first.starts_with("pending:"), "{first}");

        {
            let queue = blobs.lock().unwrap();
            assert_eq!(queue.len(), 2);
            assert_eq!(queue[0].bytes, vec![1, 2, 3]);
            assert_eq!(queue[1].mime, "image/png");
        }

        // **Across a drain**, which is the case a per-queue counter gets wrong: the queue is
        // emptied every frame, so numbering from its length starts again at zero and this
        // turn's picture resolves to the last one's.
        let drained = std::mem::take(&mut *blobs.lock().unwrap());
        assert_eq!(drained.len(), 2);
        let after = park_blob(&blobs, "image/png", vec![9]);
        assert_ne!(after, first, "a placeholder was reused after the queue was drained");
        assert_ne!(after, second);
    }

    /// A fake bundle: a directory with whichever of the two shims the test asks for, marked
    /// executable, because [`is_executable`] is the thing under test on Unix and a file with
    /// no execute bit is exactly the "copied but not runnable" case.
    fn fake_bundle(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        dir
    }

    /// The header's claim, measured: what is beside the executable is found, and what is not
    /// there is **named**. The half that matters is the second one — a bundle built before
    /// the shims were shipped has the main binary and neither of these, and the failure mode
    /// this whole path exists to end is that state being silent.
    #[test]
    fn the_shim_is_found_beside_the_executable_and_named_when_it_is_not() {
        let both = fake_bundle(&[AGENT_CLI, MCP_SERVER]);
        let shim = Shim::beside(both.path());
        assert_eq!(shim.cli(), Some(both.path().join(AGENT_CLI).as_path()));
        assert_eq!(shim.mcp(), Some(both.path().join(MCP_SERVER).as_path()));
        assert!(shim.any());
        assert_eq!(shim.gap(), None, "nothing was missing and something was reported");

        // The shape an `update.sh` that copied only the main executable leaves behind.
        let neither = fake_bundle(&[]);
        let empty = Shim::beside(neither.path());
        assert!(!empty.any());
        let gap = empty.gap().expect("a missing shim must be named");
        assert!(gap.contains(AGENT_CLI), "{gap}");
        assert!(gap.contains(MCP_SERVER), "{gap}");
        assert!(gap.contains("make-app.sh"), "the remedy is the point of the message: {gap}");

        // Half a bundle names only the half that is missing, or the user goes looking for a
        // file that is sitting right there.
        let partial = fake_bundle(&[AGENT_CLI]);
        let half = Shim::beside(partial.path());
        let gap = half.gap().expect("half a shim is still a gap");
        assert!(gap.contains(MCP_SERVER), "{gap}");
        assert!(!gap.contains(AGENT_CLI), "it named a binary that was found: {gap}");
    }

    /// The child's `PATH` must gain the shim's directory and lose nothing.
    ///
    /// Replacing it instead of prepending is the plausible mistake and the expensive one: a
    /// coding agent with no inherited `PATH` has no `git`, no toolchain, and — for a
    /// delegated CLI — not even the binary it is running as.
    #[test]
    fn the_child_gets_the_shim_first_and_keeps_the_inherited_path() {
        let dir = fake_bundle(&[AGENT_CLI]);
        let (name, value) = Shim::beside(dir.path()).path_env();
        assert_eq!(name, "PATH");

        let entries: Vec<_> = std::env::split_paths(&value).collect();
        assert_eq!(entries.first().map(PathBuf::as_path), Some(dir.path()), "{value}");

        for inherited in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            assert!(entries.contains(&inherited), "{} was dropped from PATH", inherited.display());
        }
    }

    /// The MCP document, both shapes, from one shim.
    ///
    /// Asserted structurally rather than as a string: these are guesses at two other tools'
    /// configuration formats (see `claude_cli`'s `MCP_CONFIG_FLAG`), so what is worth pinning
    /// is that the binary and the two IPC variables are *in* the document — a client that
    /// starts `velm-mcp` without them gets a server that refuses every `velm_*` tool.
    #[test]
    fn the_mcp_documents_name_the_server_and_carry_the_ipc_coordinates() {
        let dir = fake_bundle(&[AGENT_CLI, MCP_SERVER]);
        let mut spec = LaunchSpec::new(ProviderChoice::new(Provider::Claude))
            .with_shim(Shim::beside(dir.path()));
        spec.env = vec![
            (crate::mcp::IPC_ENV.into(), "/tmp/ipc.json".into()),
            (crate::mcp::AGENT_ID_ENV.into(), "board:4@7".into()),
        ];

        let config = spec.mcp_config().expect("a shipped shim configures an MCP server");
        let entry = &config["mcpServers"][MCP_SERVER_NAME];
        assert_eq!(entry["command"], dir.path().join(MCP_SERVER).display().to_string());
        assert_eq!(entry["env"][crate::mcp::IPC_ENV], "/tmp/ipc.json");
        assert_eq!(entry["env"][crate::mcp::AGENT_ID_ENV], "board:4@7");

        // ACP spells the same thing as an array of self-naming entries with `env` as pairs.
        let servers = spec.mcp_servers_acp();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], MCP_SERVER_NAME);
        let pairs = servers[0]["env"].as_array().expect("env is a list of pairs");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0]["name"], crate::mcp::IPC_ENV);
        assert_eq!(pairs[0]["value"], "/tmp/ipc.json");
    }

    /// **The degradation, which is the requirement rather than a nicety.** With no shim
    /// found there must be no MCP configuration at all — a document naming a binary that is
    /// not there is a client that fails to start its server, which on some clients fails the
    /// whole session. `[]`/`None` is what the code did before any of this existed.
    #[test]
    fn a_missing_shim_configures_no_mcp_server_rather_than_a_broken_one() {
        let bare = LaunchSpec::new(ProviderChoice::new(Provider::Claude));
        assert!(bare.mcp_config().is_none());
        assert!(bare.mcp_servers_acp().is_empty());

        // Present but incomplete: the `velm-mcp` half is what an MCP entry points at, so a
        // bundle carrying only the CLI must still configure nothing.
        let dir = fake_bundle(&[AGENT_CLI]);
        let partial = LaunchSpec::new(ProviderChoice::new(Provider::Claude))
            .with_shim(Shim::beside(dir.path()));
        assert!(partial.mcp_config().is_none(), "an MCP entry pointed at a missing binary");
        assert!(partial.mcp_servers_acp().is_empty());
    }

    /// Feature 16's other half: a local model *is* its endpoint, so the node must be able to
    /// say where it is listening. Before [`ProviderChoice::base_url`] there was no such field
    /// and `LaunchSpec::base_url` was hardcoded `None` at the one place it was built — so a
    /// local model could be chosen and never reached.
    #[test]
    fn a_node_can_say_where_its_local_model_is_listening() {
        let local = LaunchSpec::new(
            ProviderChoice::new(Provider::Local).with_base_url("http://127.0.0.1:11434/v1"),
        );
        assert_eq!(local.resolved_base_url(), Some("http://127.0.0.1:11434/v1"));

        // The caller's own override still wins over the node's, and the node's over the
        // provider default — three layers, in that order.
        let overridden =
            LaunchSpec { base_url: Some("http://elsewhere/v1".into()), ..local.clone() };
        assert_eq!(overridden.resolved_base_url(), Some("http://elsewhere/v1"));

        let claude = LaunchSpec::new(
            ProviderChoice::new(Provider::Claude).with_base_url("https://proxy.internal"),
        );
        assert_eq!(claude.resolved_base_url(), Some("https://proxy.internal"));

        // A field the user cleared is unset, not an empty endpoint — an empty base URL
        // builds a request to nowhere and reports it as a network failure.
        let cleared = LaunchSpec::new(ProviderChoice::new(Provider::Claude).with_base_url("  "));
        assert_eq!(cleared.resolved_base_url(), Some("https://api.anthropic.com"));
    }
}
