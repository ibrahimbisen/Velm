//! Agent Client Protocol: JSON-RPC 2.0 over a child process's stdio.
//!
//! `docs/07-agent-canvas.md` §5a, and **this is the transport that makes feature 17 work**.
//! `claude`, `codex` and `gemini` are already installed on the user's machine and already
//! hold the user's subscription credentials, so Velm delegates execution to that process and
//! never sees a token or a bill. **Nothing here requires an API key**, and nothing here may
//! ever start asking for one — the moment it does, a Max subscriber is paying twice.
//!
//! Velm is the **client**: it initialises a session, sends prompts, and receives streamed
//! session updates, tool-call notifications and permission requests back, then a result
//! saying why the turn stopped.
//!
//! ```text
//!   Velm ──initialize──────────────► agent
//!   Velm ──session/new─────────────► agent
//!   Velm ──session/prompt──────────► agent
//!   Velm ◄─────────session/update──── agent   (many, streamed)
//!   Velm ◄──session/request_permission agent  (a request: it waits for our answer)
//!   Velm ◄──────────────result────── agent   (stopReason)
//! ```
//!
//! # ⚠ The method names are the protocol's, and they may move
//!
//! Every wire string lives in [`method`] and [`update`] below, in one place, because they
//! belong to a specification that ships on its own schedule and cannot be verified from
//! inside this repository. Two consequences are deliberate:
//!
//! - **An unknown incoming method is ignored, never fatal.** A notification we do not
//!   recognise is a *newer agent*, not a broken session — the same posture RULE ZERO takes
//!   with an unknown value in a board file. An unknown *request* is answered
//!   `method not found`, because a request left unanswered blocks the agent forever.
//! - **The launch command is a default, not a fact.** [`crate::transport::probe_command`]
//!   runs before the spawn, so a machine without the binary gets *"`claude` is not
//!   installed"* rather than a broken feature.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{Value, json};

use crate::provider::Transport as TransportKind;
use crate::transcript::{
    RequestId, ToolCallId, TranscriptEvent, TurnId, TurnOutcome,
};
use crate::transport::{
    AgentTransport, Blobs, LaunchSpec, PendingBlob, decode_base64, park_blob, probe_command,
};
use crate::{AgentError, Result};

/// The methods Velm calls on the agent, and the ones the agent calls on Velm.
///
/// **These are the protocol's spelling, not ours.** If an agent update renames one, this is
/// the only place to change — and the symptom will be a session that initialises and then
/// says nothing, because [`handle`] ignores what it does not recognise.
pub mod method {
    /// Client → agent, once per process.
    pub const INITIALIZE: &str = "initialize";
    /// Client → agent: open a conversation with a working directory.
    pub const SESSION_NEW: &str = "session/new";
    /// Client → agent: one turn.
    pub const SESSION_PROMPT: &str = "session/prompt";
    /// Client → agent, a notification: stop the turn in flight.
    pub const SESSION_CANCEL: &str = "session/cancel";
    /// Agent → client, a notification: streamed output.
    pub const SESSION_UPDATE: &str = "session/update";
    /// Agent → client, a **request**: may I do this? Blocks the agent until answered.
    pub const REQUEST_PERMISSION: &str = "session/request_permission";
}

/// The `sessionUpdate` discriminator values on a [`method::SESSION_UPDATE`] notification.
pub mod update {
    /// Prose from the agent, addressed to the user.
    pub const AGENT_MESSAGE: &str = "agent_message_chunk";
    /// The agent's own reasoning. Raw mode only, once it reaches the painter.
    pub const AGENT_THOUGHT: &str = "agent_thought_chunk";
    /// Our own prompt, echoed back. Dropped — we wrote it.
    pub const USER_MESSAGE: &str = "user_message_chunk";
    /// A tool the agent is invoking.
    pub const TOOL_CALL: &str = "tool_call";
    /// That tool's progress and, eventually, its result.
    pub const TOOL_CALL_UPDATE: &str = "tool_call_update";
}

/// The protocol version Velm speaks.
const PROTOCOL_VERSION: u64 = 1;

/// How many lines of the agent's stderr are kept to explain an exit.
///
/// An ACP agent logs to stderr freely, so this is a **bounded tail** rather than a
/// transcript: without the bound a chatty agent left running overnight would grow Velm's
/// memory by everything it ever logged.
const STDERR_TAIL: usize = 40;

// ---------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------

/// A JSON-RPC error from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// What a request we sent came back as.
///
/// An alias rather than the type spelled out, because it appears nested three deep in
/// [`Shared::waiting`] and the shape stops being readable at that depth — and because
/// `crate::Result` pins its own error type, so the standard one has to be named in full.
type RpcAnswer = std::result::Result<Value, RpcError>;

/// What one line from the agent was.
#[derive(Debug, Clone, PartialEq)]
enum Incoming {
    /// No `id`: fire and forget.
    Notification { method: String, params: Value },
    /// Has an `id`: **it is waiting for an answer**, and the id is echoed back verbatim.
    Request { id: Value, method: String, params: Value },
    /// An answer to something we sent.
    Response { id: u64, answer: RpcAnswer },
    /// Not JSON, or not a JSON-RPC message. Ignored.
    Ignore,
}

/// Reads one line of the protocol.
///
/// The request `id` is carried as a **`Value`, verbatim**: JSON-RPC permits a string id and
/// several agents use one, so decoding it as a number would make every permission request
/// unanswerable on those agents. Only *our own* ids are known to be numbers, because we
/// allocate them.
fn decode(line: &str) -> Incoming {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return Incoming::Ignore;
    };
    let id = value.get("id").cloned().filter(|id| !id.is_null());

    if let Some(name) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        return match id {
            Some(id) => Incoming::Request { id, method: name.to_owned(), params },
            None => Incoming::Notification { method: name.to_owned(), params },
        };
    }

    let Some(id) = id.as_ref().and_then(Value::as_u64) else {
        return Incoming::Ignore;
    };
    if let Some(error) = value.get("error") {
        return Incoming::Response {
            id,
            answer: Err(RpcError {
                code: error["code"].as_i64().unwrap_or(0),
                message: error["message"].as_str().unwrap_or("the agent reported an error").to_owned(),
            }),
        };
    }
    Incoming::Response { id, answer: Ok(value.get("result").cloned().unwrap_or(Value::Null)) }
}

/// The content blocks of an update, whether one was sent or several.
fn blocks(content: &Value) -> Vec<&Value> {
    match content.as_array() {
        Some(list) => list.iter().collect(),
        None if content.is_object() => vec![content],
        None => Vec::new(),
    }
}

/// The plain text of a content block or a list of them, ignoring what is not text.
fn text_of(content: &Value) -> String {
    let mut out = String::new();
    for block in blocks(content) {
        if let Some(text) = block["text"].as_str() {
            out.push_str(text);
        } else if let Some(text) = block["content"]["text"].as_str() {
            // A tool call's content wraps its own block one level deeper.
            out.push_str(text);
        }
    }
    out
}

/// Turns one `session/update` into transcript events.
///
/// Pure but for the blob queue, so *"given this JSON line, these events come out"* is an
/// ordinary offline test — which matters here more than anywhere else in the crate, because
/// this mapping is the part most likely to be wrong about a protocol nobody here can run.
fn map_update(params: &Value, blobs: &Blobs) -> Vec<TranscriptEvent> {
    let update = &params["update"];
    let kind = update["sessionUpdate"].as_str().unwrap_or_default();
    let mut events = Vec::new();

    match kind {
        update::AGENT_MESSAGE | update::AGENT_THOUGHT => {
            for block in blocks(&update["content"]) {
                match block["type"].as_str().unwrap_or("text") {
                    "image" => {
                        // The bytes go out through the pending-blob queue; this crate must
                        // not write to the blob store. See `transport::PendingBlob`.
                        if let Some(data) = block["data"].as_str()
                            && let Some(bytes) = decode_base64(data)
                        {
                            let mime = block["mimeType"].as_str().unwrap_or("image/png");
                            let id = park_blob(blobs, mime, bytes);
                            events.push(TranscriptEvent::Image {
                                blob: id,
                                caption: block["caption"].as_str().map(str::to_owned),
                            });
                        }
                    }
                    _ => {
                        let text = block["text"].as_str().unwrap_or_default();
                        if text.is_empty() {
                            continue;
                        }
                        events.push(if kind == update::AGENT_THOUGHT {
                            TranscriptEvent::Thought { text: text.to_owned() }
                        } else {
                            TranscriptEvent::Text { text: text.to_owned() }
                        });
                    }
                }
            }
        }
        update::TOOL_CALL => {
            let id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
            let name = update["title"]
                .as_str()
                .or_else(|| update["kind"].as_str())
                .unwrap_or("a tool")
                .to_owned();
            let input = match &update["rawInput"] {
                Value::Null => text_of(&update["content"]),
                raw => raw.to_string(),
            };
            events.push(TranscriptEvent::ToolCall { id: ToolCallId(id), name, input });
        }
        update::TOOL_CALL_UPDATE => {
            let status = update["status"].as_str().unwrap_or_default();
            // `in_progress` and `pending` are progress, not a result: emitting one would put
            // a "tool succeeded" line in the transcript before the tool had done anything.
            if matches!(status, "completed" | "failed") {
                let id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
                let output = match &update["rawOutput"] {
                    Value::Null => text_of(&update["content"]),
                    raw => raw.to_string(),
                };
                events.push(TranscriptEvent::ToolResult {
                    id: ToolCallId(id),
                    output,
                    ok: status == "completed",
                });
            }
        }
        // `plan`, `usage_update`, `available_commands_update`, `current_mode_update`, our own
        // prompt echoed back, and anything a later agent invents: nothing the transcript
        // needs, and **ignored rather than fatal**.
        _ => {}
    }
    events
}

/// How a turn stopped, from the `session/prompt` result.
///
/// An unrecognised reason is a finished answer. The alternative — treating it as a failure —
/// would put a red error on the node the first time an agent added a stop reason.
fn map_stop_reason(result: &Value) -> TurnOutcome {
    match result["stopReason"].as_str().unwrap_or("end_turn") {
        "cancelled" => TurnOutcome::Cancelled,
        "max_tokens" => {
            TurnOutcome::Exhausted { message: "the agent reached its context limit".into() }
        }
        "max_turn_requests" => {
            TurnOutcome::Exhausted { message: "the agent reached its own request budget".into() }
        }
        "refusal" => TurnOutcome::Failed { message: "the agent declined this request".into() },
        _ => TurnOutcome::Completed,
    }
}

/// The two lines a permission request shows: what is being asked, and the detail behind it.
fn permission_summary(params: &Value) -> (String, String) {
    let call = &params["toolCall"];
    let summary = call["title"]
        .as_str()
        .or_else(|| call["kind"].as_str())
        .unwrap_or("run a tool")
        .to_owned();
    let detail = match &call["rawInput"] {
        Value::Null => text_of(&call["content"]),
        raw => raw.to_string(),
    };
    (summary, detail)
}

/// Picks which of the agent's offered options a yes or a no means.
///
/// The options are the agent's, not ours: it offers a list, each with an `optionId` and a
/// `kind`, and the answer names one of them. **Once beats always** — a user clicking Allow
/// on one dialog has not agreed to every future one, and an interface that silently upgraded
/// a single yes into a standing grant would be the worst kind of surprise.
///
/// A list with no recognisable kinds still answers: the first option for yes and the last for
/// no, which is the order every such list is written in.
fn pick_option(options: &Value, allowed: bool) -> Option<String> {
    let list = options.as_array()?;
    let wanted = if allowed { "allow" } else { "reject" };

    let of_kind = |suffix: &str| {
        list.iter().find(|option| {
            option["kind"].as_str().is_some_and(|kind| kind == format!("{wanted}_{suffix}"))
        })
    };
    let matching = of_kind("once")
        .or_else(|| of_kind("always"))
        .or_else(|| {
            list.iter().find(|option| {
                option["kind"].as_str().is_some_and(|kind| kind.starts_with(wanted))
            })
        })
        .or_else(|| if allowed { list.first() } else { list.last() })?;

    matching["optionId"].as_str().map(str::to_owned)
}

// ---------------------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------------------

/// A permission request the agent is blocked on.
struct Pending {
    /// The agent's own JSON-RPC id, echoed back verbatim.
    rpc_id: Value,
    options: Value,
}

/// Everything the reader thread and the turn thread both touch.
struct Shared {
    stdin: Mutex<Option<ChildStdin>>,
    /// Requests we sent and are waiting on, by our own id.
    waiting: Mutex<HashMap<u64, Sender<RpcAnswer>>>,
    /// Permission requests the agent is blocked on, by the id the user will answer with.
    permissions: Mutex<HashMap<String, Pending>>,
    next_id: AtomicU64,
    session: Mutex<Option<String>>,
    /// Whether the resolved system context has been delivered. It rides the first prompt —
    /// see [`AcpTransport::send_prompt`].
    context_sent: AtomicBool,
    alive: AtomicBool,
    cancelled: AtomicBool,
    stderr: Mutex<VecDeque<String>>,
    blobs: Blobs,
    events: Sender<TranscriptEvent>,
    cwd: String,
    system: String,
    /// Velm's own MCP server, in the shape `session/new` asks for. Empty when `velm-mcp` was
    /// not found beside the application — see [`LaunchSpec::mcp_servers_acp`], and see
    /// [`run_turn`] for what happens when an agent refuses the entry.
    mcp_servers: Vec<Value>,
}

impl Shared {
    fn write_line(&self, message: &Value) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let stdin = guard.as_mut().ok_or_else(|| AgentError::Transport {
            transport: "acp",
            message: "the agent's input has been closed".into(),
        })?;
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| AgentError::Transport {
                transport: "acp",
                message: error.to_string(),
            })
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Sends a request and **blocks this thread** until the answer arrives.
    ///
    /// Only ever called from a worker, never from the frame loop. There is no timeout on
    /// purpose: an agentic turn legitimately runs for an hour, a clock would have to guess
    /// how long is too long, and this crate does not read one. Liveness comes from the other
    /// end instead — when the reader thread sees the process end it fails everything still
    /// waiting, so a dead agent wakes the caller immediately rather than at some deadline.
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, wait) = channel();
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id, answer);

        if let Err(error) = self.write_line(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        ) {
            self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
            return Err(error);
        }

        match wait.recv() {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(AgentError::Transport {
                transport: "acp",
                message: format!("the agent refused `{method}`: {}", error.message),
            }),
            Err(_) => Err(AgentError::Transport {
                transport: "acp",
                message: format!("the agent stopped before answering `{method}`"),
            }),
        }
    }

    fn emit(&self, event: TranscriptEvent) {
        let _ = self.events.send(event);
    }

    /// Answers a permission request, if it is still outstanding.
    fn answer(&self, id: &RequestId, allowed: bool) {
        let pending = {
            let mut held =
                self.permissions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            held.remove(&id.0)
        };
        let Some(pending) = pending else {
            return;
        };
        let outcome = match pick_option(&pending.options, allowed) {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            // Nothing to select is not the same as "no": it is a request we cannot answer in
            // the terms it was asked, and `cancelled` is the protocol's word for that.
            None => json!({ "outcome": "cancelled" }),
        };
        let _ = self.write_line(&json!({
            "jsonrpc": "2.0",
            "id": pending.rpc_id,
            "result": { "outcome": outcome },
        }));
        // Emitted here rather than by the caller, so the answer lands in the one ordered
        // stream the sidecar records — see `AgentTransport::answer_permission`.
        self.emit(TranscriptEvent::PermissionAnswer { id: id.clone(), allowed });
    }

    /// Fails everything still waiting. Called when the process ends.
    fn abandon(&self, why: &str) {
        let waiting = std::mem::take(
            &mut *self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for sender in waiting.into_values() {
            let _ = sender.send(Err(RpcError { code: 0, message: why.to_owned() }));
        }
        let outstanding: Vec<String> = self
            .permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for id in outstanding {
            // Not `answer(.., false)`: the agent is gone, so there is nobody to tell, and a
            // `PermissionAnswer` in the transcript would claim the user refused something
            // they were never shown the outcome of.
            self.permissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
        }
    }
}

/// An agent process speaking ACP.
pub struct AcpTransport {
    shared: Arc<Shared>,
    child: Child,
    reader: Option<JoinHandle<()>>,
    errors: Option<JoinHandle<()>>,
    turn: Option<JoinHandle<()>>,
    shut_down: bool,
}

impl std::fmt::Debug for AcpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpTransport")
            .field("alive", &self.shared.alive.load(Ordering::Relaxed))
            .field("session", &self.shared.session.lock().is_ok())
            .finish_non_exhaustive()
    }
}

impl AcpTransport {
    /// Spawns the agent and starts reading it. **Sends nothing yet** — the protocol
    /// handshake happens on the first prompt, so a board that opens with agent nodes on it
    /// costs one process and no conversation.
    pub fn start(spec: &LaunchSpec, events: Sender<TranscriptEvent>) -> Result<Self> {
        let command = spec.resolved_command().filter(|name| !name.is_empty()).ok_or_else(|| {
            AgentError::Refused(format!(
                "{} has no agent command to delegate to — it can only be reached over its API",
                spec.provider.provider.label()
            ))
        })?;
        let program = probe_command(command)?;

        let cwd = spec
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();

        let mut process = Command::new(&program);
        process
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process.current_dir(&cwd);
        for (key, value) in &spec.env {
            process.env(key, value);
        }

        let mut child = process.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AgentError::MissingCommand { command: command.to_owned() }
            } else {
                AgentError::Transport { transport: "acp", message: error.to_string() }
            }
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            waiting: Mutex::new(HashMap::new()),
            permissions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
            context_sent: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            stderr: Mutex::new(VecDeque::new()),
            blobs: Blobs::default(),
            events,
            cwd: cwd.to_string_lossy().into_owned(),
            system: spec.system_context.clone(),
            mcp_servers: spec.mcp_servers_acp(),
        });

        let reader = stdout.map(|stdout| {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("velm-acp".into())
                .spawn(move || pump(BufReader::new(stdout), &shared))
                .map_err(AgentError::Io)
        });
        let reader = match reader {
            Some(handle) => Some(handle?),
            None => None,
        };

        let errors = stderr.map(|stderr| {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("velm-acp-log".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
                        let mut tail = shared
                            .stderr
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if tail.len() >= STDERR_TAIL {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })
                .map_err(AgentError::Io)
        });
        let errors = match errors {
            Some(handle) => Some(handle?),
            None => None,
        };

        Ok(Self { shared, child, reader, errors, turn: None, shut_down: false })
    }

    fn busy(&self) -> bool {
        self.turn.as_ref().is_some_and(|handle| !handle.is_finished())
    }
}

impl AgentTransport for AcpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Acp
    }

    /// Initialises the session if it is the first prompt, then sends the turn.
    ///
    /// The **resolved system context rides the first prompt** as a leading text block. ACP
    /// has no system-prompt field — it delegates to an agent that has its own idea of who it
    /// is — so the context is delivered the one way the protocol offers, and only once: a
    /// context repeated on every turn would grow the agent's conversation without bound and
    /// re-assert a role it already has.
    fn send_prompt(&mut self, turn: TurnId, prompt: &str) -> Result<()> {
        if self.busy() {
            return Err(AgentError::Refused(
                "this agent is still working — the prompt was not sent".into(),
            ));
        }
        if !self.shared.alive.load(Ordering::Relaxed) {
            return Err(AgentError::Transport {
                transport: "acp",
                message: "the agent process has ended".into(),
            });
        }
        drop(self.turn.take());
        self.shared.cancelled.store(false, Ordering::Relaxed);

        let shared = Arc::clone(&self.shared);
        let prompt = prompt.to_owned();
        // ⚠ **`TurnStarted` is emitted by the thread, not here.** Trap 11's shape at a
        // different layer: `TurnStarted`/`TurnEnded` is a paired begin/end and `.spawn(…)?`
        // sat between them, so a failed spawn left a `TurnStarted` in the channel that
        // nothing would ever close — the node reads `Status::Running` for good, with no turn
        // behind it. Inside the closure the pair is structural: the same thread emits both,
        // in order, or neither exists at all.
        let handle = std::thread::Builder::new()
            .name("velm-acp-turn".into())
            .spawn(move || {
                shared.emit(TranscriptEvent::TurnStarted {
                    turn,
                    prompt: prompt.clone(),
                });
                let outcome = match run_turn(&shared, &prompt) {
                    Ok(outcome) => outcome,
                    Err(error) => TurnOutcome::Failed { message: error.to_string() },
                };
                let outcome = if shared.cancelled.load(Ordering::Relaxed) {
                    TurnOutcome::Cancelled
                } else {
                    outcome
                };
                shared.emit(TranscriptEvent::TurnEnded { turn, outcome });
            })
            .map_err(AgentError::Io)?;
        self.turn = Some(handle);
        Ok(())
    }

    fn cancel(&mut self) -> Result<()> {
        self.shared.cancelled.store(true, Ordering::Relaxed);
        let session = self
            .shared
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(session) = session else {
            return Ok(());
        };
        self.shared.notify(method::SESSION_CANCEL, json!({ "sessionId": session }))
    }

    fn answer_permission(&mut self, id: &RequestId, allowed: bool) -> Result<()> {
        self.shared.answer(id, allowed);
        Ok(())
    }

    fn take_pending_blobs(&mut self) -> Vec<PendingBlob> {
        std::mem::take(
            &mut *self.shared.blobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// Ends the session and releases the process.
    ///
    /// ⚠ **Outstanding permission requests are cancelled first.** An agent blocked on
    /// `session/request_permission` is waiting for a reply on a pipe we are about to close;
    /// killing it without answering is how a child ends up unkillable on some platforms and,
    /// worse, how a *detached* one waits forever.
    fn shutdown(&mut self) -> Result<()> {
        if self.shut_down {
            return Ok(());
        }
        self.shut_down = true;

        let outstanding: Vec<String> = self
            .shared
            .permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for id in outstanding {
            self.shared.answer(&RequestId(id), false);
        }

        // Closing stdin is the polite request to exit; the kill is what makes shutdown
        // bounded when the agent ignores it.
        drop(
            self.shared
                .stdin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        let killed = self.child.kill();
        let _ = self.child.wait();
        self.shared.alive.store(false, Ordering::Relaxed);
        self.shared.abandon("the agent session was closed");

        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(errors) = self.errors.take() {
            let _ = errors.join();
        }
        // The turn thread is deliberately **not** joined: it may be blocked on a request the
        // dead agent will never answer, and `abandon` has already woken it.
        drop(self.turn.take());

        killed.map_err(|error| AgentError::Transport {
            transport: "acp",
            message: error.to_string(),
        })
    }
}

impl Drop for AcpTransport {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Opens the ACP session, offering Velm's MCP server and **giving it up rather than the
/// session** if the agent will not take it.
///
/// # Why the retry, rather than getting it right
///
/// `mcpServers` used to be hardcoded `[]`, which is why nothing in `docs/07` §6 was reachable
/// from an ACP agent. Filling it in means sending another tool a document shaped the way the
/// protocol describes — and the two agents this transport is a default for, `codex` and
/// `gemini`, have **never been run from here** (see [`crate::provider`]). So the entry is a
/// guess, and a guess on `session/new` is not a missing feature: it is the handshake, and a
/// refusal there means the node never starts at all.
///
/// One retry with the empty list restores exactly the behaviour that shipped before, so the
/// worst case is the state we were already in. It is attempted **once** and only after a
/// refusal, so an agent that is simply broken fails on its own terms rather than twice.
fn open_session(shared: &Shared) -> Result<Value> {
    if shared.mcp_servers.is_empty() {
        return shared.request(method::SESSION_NEW, json!({ "cwd": shared.cwd, "mcpServers": [] }));
    }

    let offered = shared.request(
        method::SESSION_NEW,
        json!({ "cwd": shared.cwd, "mcpServers": shared.mcp_servers }),
    );
    match offered {
        Ok(opened) => Ok(opened),
        Err(refused) => {
            // Worth saying: the agent is about to run without any of the board verbs, and the
            // silence would otherwise be indistinguishable from an agent that has them and
            // chooses not to use them.
            shared.emit(TranscriptEvent::Error {
                message: format!(
                    "this agent would not accept Velm's tools, so it is running without them \
                     ({refused}). It can still answer; it cannot message other agents or read \
                     the board's notes."
                ),
            });
            shared.request(method::SESSION_NEW, json!({ "cwd": shared.cwd, "mcpServers": [] }))
        }
    }
}

/// The handshake and one turn, on the turn thread.
fn run_turn(shared: &Shared, prompt: &str) -> Result<TurnOutcome> {
    let session = {
        let existing = shared
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match existing {
            Some(session) => session,
            None => {
                shared.request(
                    method::INITIALIZE,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "clientInfo": { "name": "Velm", "version": env!("CARGO_PKG_VERSION") },
                        // Declared false because we answer `method not found` to both — an
                        // agent that believed otherwise would block on a call we refuse.
                        "clientCapabilities": {
                            "fs": { "readTextFile": false, "writeTextFile": false }
                        },
                    }),
                )?;
                let opened = open_session(shared)?;
                let id = opened["sessionId"]
                    .as_str()
                    .ok_or_else(|| AgentError::Transport {
                        transport: "acp",
                        message: "the agent opened a session with no id".into(),
                    })?
                    .to_owned();
                *shared.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(id.clone());
                id
            }
        }
    };

    let mut content = Vec::with_capacity(2);
    if !shared.system.is_empty()
        && !shared.context_sent.swap(true, Ordering::Relaxed)
    {
        content.push(json!({ "type": "text", "text": shared.system }));
    }
    content.push(json!({ "type": "text", "text": prompt }));

    let result = shared.request(
        method::SESSION_PROMPT,
        json!({ "sessionId": session, "prompt": content }),
    )?;
    Ok(map_stop_reason(&result))
}

/// The reader thread: one JSON object per line, for the life of the process.
fn pump(reader: impl BufRead, shared: &Shared) {
    for line in reader.lines().map_while(std::result::Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        handle(decode(&line), shared);
    }

    shared.alive.store(false, Ordering::Relaxed);
    let tail: Vec<String> = shared
        .stderr
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect();
    shared.abandon("the agent process ended");
    // Only worth saying when the agent left something behind: a clean exit after the user
    // closed the node is not an error, and reporting one would put a red line on every node
    // that was ever shut down.
    if !tail.is_empty() {
        shared.emit(TranscriptEvent::Error {
            message: format!("the agent process ended: {}", tail.join(" · ")),
        });
    }
}

/// What to do with one decoded line.
fn handle(incoming: Incoming, shared: &Shared) {
    match incoming {
        Incoming::Notification { method: name, params } => {
            if name == method::SESSION_UPDATE {
                for event in map_update(&params, &shared.blobs) {
                    shared.emit(event);
                }
            }
            // Anything else is a newer agent talking about something we do not draw.
        }
        Incoming::Request { id, method: name, params } => {
            if name == method::REQUEST_PERMISSION {
                let request = RequestId(format!("acp-{id}"));
                let (summary, detail) = permission_summary(&params);
                shared
                    .permissions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        request.0.clone(),
                        Pending { rpc_id: id, options: params["options"].clone() },
                    );
                shared.emit(TranscriptEvent::PermissionRequest {
                    id: request,
                    summary,
                    detail,
                });
                return;
            }
            // ⚠ A request **must** be answered, even one we do not implement: the agent is
            // blocked on it, and silence is a hang rather than a refusal. `fs/read_text_file`
            // and friends land here, which is consistent with the capabilities we declared.
            let _ = shared.write_line(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Velm does not implement `{name}`") },
            }));
        }
        Incoming::Response { id, answer } => {
            let waiting = shared
                .waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            if let Some(sender) = waiting {
                let _ = sender.send(answer);
            }
        }
        Incoming::Ignore => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response's id may be ours (a number) and a *request's* may be the agent's (a string
    /// on several of them). Decoding the agent's as a number is what makes every permission
    /// request unanswerable on those agents, and it is invisible until you run one.
    #[test]
    fn a_line_is_told_apart_by_its_id_and_a_string_id_survives() {
        let notification = decode(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#,
        );
        assert!(matches!(notification, Incoming::Notification { .. }));

        let request = decode(
            r#"{"jsonrpc":"2.0","id":"req-7","method":"session/request_permission","params":{}}"#,
        );
        match request {
            Incoming::Request { id, method: name, .. } => {
                assert_eq!(id, json!("req-7"), "a string id was not carried verbatim");
                assert_eq!(name, method::REQUEST_PERMISSION);
            }
            other => panic!("a request with a string id decoded as {other:?}"),
        }

        let response = decode(r#"{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}"#);
        assert_eq!(
            response,
            Incoming::Response { id: 4, answer: Ok(json!({ "stopReason": "end_turn" })) }
        );

        let failed = decode(r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"no"}}"#);
        match failed {
            Incoming::Response { id, answer: Err(error) } => {
                assert_eq!(id, 5);
                assert_eq!(error.code, -32601);
            }
            other => panic!("an error response decoded as {other:?}"),
        }

        // A line that is not JSON at all — agents print banners to stdout — must not take
        // the session down.
        assert_eq!(decode("Starting agent…"), Incoming::Ignore);
        assert_eq!(decode(""), Incoming::Ignore);
    }

    /// The mapping, against the lines the protocol actually sends. This is the part most
    /// likely to be wrong and the only part of it an offline test can reach.
    #[test]
    fn a_session_update_maps_onto_the_transcript() {
        let blobs = Blobs::default();
        let map = |json: &str| {
            let value: Value = serde_json::from_str(json).unwrap();
            map_update(&value, &blobs)
        };

        assert_eq!(
            map(
                r#"{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"the answer"}}}"#
            ),
            vec![TranscriptEvent::Text { text: "the answer".into() }]
        );

        assert_eq!(
            map(
                r#"{"update":{"sessionUpdate":"agent_thought_chunk",
                    "content":{"type":"text","text":"weighing it"}}}"#
            ),
            vec![TranscriptEvent::Thought { text: "weighing it".into() }]
        );

        let call = map(
            r#"{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read main.rs",
                "kind":"read","status":"pending","rawInput":{"path":"src/main.rs"}}}"#,
        );
        assert_eq!(call.len(), 1);
        match &call[0] {
            TranscriptEvent::ToolCall { id, name, input } => {
                assert_eq!(id, &ToolCallId("t1".into()));
                assert_eq!(name, "Read main.rs");
                assert!(input.contains("src/main.rs"), "{input}");
            }
            other => panic!("a tool call mapped to {other:?}"),
        }

        // Progress is not a result. Emitting one would put "tool succeeded" in the
        // transcript before the tool had done anything.
        assert!(
            map(r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress"}}"#)
                .is_empty()
        );
        assert_eq!(
            map(
                r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1",
                    "status":"failed","content":[{"type":"content","content":{"type":"text","text":"no such file"}}]}}"#
            ),
            vec![TranscriptEvent::ToolResult {
                id: ToolCallId("t1".into()),
                output: "no such file".into(),
                ok: false,
            }]
        );

        // Our own prompt echoed back is not the agent talking, and an update kind from a
        // later agent is ignored rather than fatal.
        assert!(map(r#"{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"go"}}}"#).is_empty());
        assert!(map(r#"{"update":{"sessionUpdate":"a_future_kind","content":{}}}"#).is_empty());
        assert!(map(r#"{"update":{}}"#).is_empty());
    }

    /// An image goes out as bytes on the pending queue and a **placeholder** in the event.
    /// This crate must not write to the blob store, so an `Image` event carrying anything
    /// hash-shaped here would be a lie the painter then fails to resolve.
    #[test]
    fn an_image_is_parked_as_bytes_and_referenced_by_placeholder() {
        let blobs = Blobs::default();
        // "hello" in base64.
        let value: Value = serde_json::from_str(
            r#"{"update":{"sessionUpdate":"agent_message_chunk",
                "content":{"type":"image","mimeType":"image/png","data":"aGVsbG8="}}}"#,
        )
        .unwrap();
        let events = map_update(&value, &blobs);

        let queued = blobs.lock().unwrap();
        assert_eq!(queued.len(), 1, "the bytes were not parked");
        assert_eq!(queued[0].bytes, b"hello");
        assert_eq!(queued[0].mime, "image/png");
        match &events[0] {
            TranscriptEvent::Image { blob, .. } => assert_eq!(blob, &queued[0].id),
            other => panic!("an image mapped to {other:?}"),
        }
    }

    /// ⚠ **This test asserted the bug**, and it is the reason the bug survived: its second and
    /// fourth lines *required* the unpadded and line-wrapped forms to decode, under a comment
    /// reading *"wrapped base64 is still base64"*. This file had its own lenient copy of a
    /// decoder whose doc comment claimed to be strict, and the same leniency turned the single
    /// character `"a"` into `Some(vec![])` — a zero-byte blob, parked and written to the
    /// content-addressed store as an agent's picture.
    ///
    /// There is one decoder now (`crate::ipc::decode_base64`), and it refuses every shape a
    /// truncated or spliced payload takes. ACP sends `data` as a single unwrapped token, so
    /// nothing legitimate on this path is lost.
    #[test]
    fn base64_refuses_the_shapes_a_truncated_or_spliced_payload_takes() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("YW55IGNhcm5hbCBwbGVhc3VyZQ==").unwrap(), b"any carnal pleasure");

        assert!(decode_base64("not base64!").is_none());
        // ⚠ This line used to be `decode_base64("").unwrap() == b""`, one line below the one
        // that refuses a zero-byte blob from `"a"` — the same defect, asserted as correct in
        // the half nobody had looked at. Nothing here checks `is_empty()` before parking the
        // bytes, so an empty `data` field went into the store as a picture.
        assert!(decode_base64("").is_none(), "an empty payload is not a picture");
        assert!(decode_base64("a").is_none(), "one character decoded to a zero-byte blob");
        assert!(decode_base64("aGVsbG8").is_none(), "an unpadded length is a truncated payload");
        assert!(decode_base64("aGVs\nbG8=").is_none(), "whitespace inside the payload");
    }

    /// Every transport reports the same four outcomes, so the node says the same thing
    /// whichever kind of agent is behind it. An unrecognised reason is a finished answer.
    #[test]
    fn stop_reasons_map_onto_the_same_outcomes_as_the_other_transports() {
        let stopped = |reason: &str| map_stop_reason(&json!({ "stopReason": reason }));
        assert_eq!(stopped("end_turn"), TurnOutcome::Completed);
        assert_eq!(stopped("cancelled"), TurnOutcome::Cancelled);
        assert!(matches!(stopped("max_tokens"), TurnOutcome::Exhausted { .. }));
        assert!(matches!(stopped("max_turn_requests"), TurnOutcome::Exhausted { .. }));
        assert!(matches!(stopped("refusal"), TurnOutcome::Failed { .. }));
        assert_eq!(stopped("something_new"), TurnOutcome::Completed);
        // A result with no reason at all is an answer that finished.
        assert_eq!(map_stop_reason(&json!({})), TurnOutcome::Completed);
    }

    /// **Once beats always.** A user clicking Allow on one dialog has not agreed to every
    /// future one, and an interface that silently upgraded a single yes into a standing
    /// grant is the worst kind of surprise. The `always` fallback exists for an agent that
    /// offers nothing else.
    #[test]
    fn a_yes_picks_the_narrowest_allow_and_a_no_picks_a_reject() {
        let full = json!([
            { "optionId": "a", "name": "Allow always", "kind": "allow_always" },
            { "optionId": "b", "name": "Allow once", "kind": "allow_once" },
            { "optionId": "c", "name": "Reject once", "kind": "reject_once" },
        ]);
        assert_eq!(pick_option(&full, true).as_deref(), Some("b"));
        assert_eq!(pick_option(&full, false).as_deref(), Some("c"));

        let only_always = json!([
            { "optionId": "x", "kind": "allow_always" },
            { "optionId": "y", "kind": "reject_always" },
        ]);
        assert_eq!(pick_option(&only_always, true).as_deref(), Some("x"));
        assert_eq!(pick_option(&only_always, false).as_deref(), Some("y"));

        // An agent that offers options with no kind at all still gets an answer: yes takes
        // the first, no takes the last, which is the order every such list is written in.
        let unnamed = json!([{ "optionId": "yes" }, { "optionId": "no" }]);
        assert_eq!(pick_option(&unnamed, true).as_deref(), Some("yes"));
        assert_eq!(pick_option(&unnamed, false).as_deref(), Some("no"));

        assert_eq!(pick_option(&json!([]), true), None);
        assert_eq!(pick_option(&Value::Null, true), None);
    }

    #[test]
    fn a_permission_request_says_what_is_being_asked() {
        let params: Value = serde_json::from_str(
            r#"{"sessionId":"s","toolCall":{"title":"Write src/main.rs","kind":"edit",
                "rawInput":{"path":"src/main.rs"}},
                "options":[{"optionId":"a","kind":"allow_once"}]}"#,
        )
        .unwrap();
        let (summary, detail) = permission_summary(&params);
        assert_eq!(summary, "Write src/main.rs");
        assert!(detail.contains("src/main.rs"), "{detail}");

        // A request with nothing but a kind still names something the user can act on.
        let bare: Value = serde_json::from_str(r#"{"toolCall":{"kind":"execute"}}"#).unwrap();
        assert_eq!(permission_summary(&bare).0, "execute");
    }

    /// A `session/prompt` sent before the handshake is a protocol error every agent rejects,
    /// so the order in `run_turn` is behaviour rather than tidiness. Asserted through the
    /// method table, which is the only part of it a test with no agent can see.
    #[test]
    fn the_protocol_methods_are_spelled_in_one_place() {
        assert_eq!(method::INITIALIZE, "initialize");
        assert_eq!(method::SESSION_NEW, "session/new");
        assert_eq!(method::SESSION_PROMPT, "session/prompt");
        assert_eq!(method::SESSION_CANCEL, "session/cancel");
        assert_eq!(method::SESSION_UPDATE, "session/update");
        assert_eq!(method::REQUEST_PERMISSION, "session/request_permission");
    }
}
