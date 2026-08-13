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
#[derive(Debug, Clone, Default)]
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

    /// The API base, resolving `None` through the provider.
    pub fn resolved_base_url(&self) -> Option<&str> {
        self.base_url.as_deref().or_else(|| self.provider.provider.default_base_url())
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

/// Base64, for the image bytes an agent sends inline.
///
/// Hand-rolled to keep this crate's dependency list at the `docs/07` §1 names, and short
/// enough to read: four characters in, three bytes out, whitespace and padding skipped.
/// Answers `None` on anything that is not base64 rather than producing bytes that are not the
/// picture — a half-decoded image is a texture upload of garbage, not a smaller picture.
///
/// It lives here rather than in one transport because **two protocols carry pictures in
/// different envelopes and the same encoding**: ACP's flat `data`/`mimeType` and Anthropic's
/// `source.data`/`source.media_type`. [`acp`] still has its own private copy from before this
/// one existed; collapsing it onto this is a one-line edit for whoever owns that file next.
pub(crate) fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = sextet(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

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
    #[test]
    fn base64_decodes_a_picture_and_refuses_what_is_not_one() {
        assert_eq!(decode_base64("aGk=").as_deref(), Some(&b"hi"[..]));
        // Padding and line breaks are both legal in a wire payload.
        assert_eq!(decode_base64("aGVsbG8gd29ybGQ=").as_deref(), Some(&b"hello world"[..]));
        assert_eq!(decode_base64("aGVs\nbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(decode_base64(""), Some(Vec::new()));
        assert_eq!(decode_base64("not base64!"), None);
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
}
