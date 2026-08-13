//! The `claude` CLI's own stream-json protocol, over a child process's stdio.
//!
//! **This is the transport that makes feature 17 work.** `claude` is already installed on the
//! user's machine and already holds their subscription credentials, so Velm delegates
//! execution to that process and never sees a token or a bill. Nothing here requires an API
//! key and nothing here may ever start asking for one — the moment it does, a Max subscriber
//! is paying twice.
//!
//! # ⚠ What supersedes what, and why this file exists at all
//!
//! [`super::acp`] was written against the Agent Client Protocol from recall, because nothing
//! could be verified at the time. **`claude` does not speak ACP.** Measured on this machine on
//! **2026-08-13**, against the version on `PATH`: it speaks a line-delimited JSON protocol of
//! its own, one object per line, and every claim in this module header was produced by running
//! it rather than remembered.
//!
//! ```text
//!   claude -p --input-format stream-json --output-format stream-json --verbose
//!
//!   Velm ──{"type":"user","message":{…}}──────────────► claude   (one line = one turn)
//!   Velm ◄──{"type":"system","subtype":"init",…}────── claude   (model, capabilities — per turn)
//!   Velm ◄──{"type":"assistant","message":{…}}──────── claude   (Anthropic's own message shape)
//!   Velm ◄──{"type":"user","message":{…tool_result…}}─ claude   (what a tool answered)
//!   Velm ◄──{"type":"result","subtype":"success",…}─── claude   (the turn boundary)
//!   Velm ──{"type":"control_request",…"interrupt"}───► claude   (cancel)
//!   Velm ◄──{"type":"control_response",…}───────────── claude
//! ```
//!
//! ## Measured, not assumed
//!
//! - **One process serves many turns.** Two user messages queued on one stdin produced two
//!   `result` lines with the same `session_id`, and the second read the first out of the
//!   prompt cache (`cache_read_input_tokens: 6272`) — so the conversation is retained and a
//!   node is one child process, not one per turn.
//! - **`system`/`init` arrives once per *turn*, not once per process.** Both turns above
//!   emitted one. So it is a fact to record, never an event, and repeats are ordinary.
//! - **Interrupt is a `control_request`, and it is answered.** Idle:
//!   `{"type":"control_response","response":{"subtype":"success","request_id":"req_1",
//!   "response":{"still_queued":[]}}}` — that is the `interrupt_receipt_v1` and
//!   `interrupt_cancel_queued_v1` the `init` line advertises. **Mid-turn** it is answered the
//!   same way, then a `user` line reading *"[Request interrupted by user]"*, then a `result`
//!   with `"is_error": true` and `"stop_reason": null`. The **process stays alive** and takes
//!   the next prompt, which is why cancelling does not kill the child: killing it would throw
//!   away the conversation the two-turn measurement just proved is there. Only
//!   [`ClaudeCli::shutdown`] kills.
//! - **The input message shape is accepted verbatim**:
//!   `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]}}`.
//!
//! ## Inferred, and marked as such at each site
//!
//! The `result` subtypes other than `success`, and the `can_use_tool` control request's
//! response shape. Both are recalled from the Agent SDK's own protocol rather than measured;
//! the code is written so that being wrong about either degrades to a named failure rather
//! than to a hang. An unknown `type`, an unknown `subtype` and a line that is not JSON are all
//! **ignored** — this CLI ships on its own schedule, so an unrecognised line is a newer
//! version, not a broken session. That is the same posture RULE ZERO takes with an unknown
//! value in a board file.
//!
//! # Credentials: never set, never stripped
//!
//! `ANTHROPIC_API_KEY` is **never set** by this transport — the CLI holds the user's OAuth
//! credentials and that is the entire point. It is also deliberately **not removed** from the
//! inherited environment: stripping it would silently change the CLI's authentication from
//! what the same command does in the user's own terminal, and an enterprise Bedrock/Vertex
//! setup authenticates through its own variables. [`LaunchSpec::api_key`] is ignored here.
//!
//! ⚠ **`--bare` must never be passed.** Its own help text says Anthropic auth becomes
//! *"strictly `ANTHROPIC_API_KEY` or `apiKeyHelper`"* and that OAuth and the keychain are
//! never read — i.e. it disables the exact path this feature exists to use. If a future change
//! wants a leaner child, `--safe-mode` is the flag that trims customisation without touching
//! authentication.
//!
//! The `init` line carries `apiKeySource`, which is how a session that *is* being billed to a
//! key could be detected. Nothing acts on it today; it is recorded in [`Facts`] so that
//! [`crate::provider::ProviderChoice::is_metered`] can become honest at runtime later without
//! a new wire read.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{Value, json};

use crate::provider::Transport as TransportKind;
use crate::transcript::{RequestId, ToolCallId, TranscriptEvent, TurnId, TurnOutcome};
use crate::transport::{
    AgentTransport, Blobs, LaunchSpec, PendingBlob, decode_base64, park_blob, probe_command,
};
use crate::{AgentError, Result};

/// Every wire string the protocol spells, in one place.
///
/// They belong to a tool that ships on its own schedule. The symptom of one moving is a
/// session that runs and says nothing, because [`read_line`] ignores what it does not
/// recognise — which is the right trade against a session that dies on a line it could have
/// skipped, but it is worth knowing where to look.
pub mod wire {
    /// The CLI talking about itself: `init`, hooks, token estimates.
    pub const SYSTEM: &str = "system";
    /// A message from the model, in Anthropic's own message shape.
    pub const ASSISTANT: &str = "assistant";
    /// A message *to* the model. Without `--replay-user-messages` this is never our own
    /// prompt coming back — it is a tool's answer, or the CLI's own interruption notice.
    pub const USER: &str = "user";
    /// The turn boundary.
    pub const RESULT: &str = "result";
    /// The subscription's rate-limit window, reported unasked.
    pub const RATE_LIMIT: &str = "rate_limit_event";
    /// The CLI asking *us* something. It is blocked until answered.
    pub const CONTROL_REQUEST: &str = "control_request";
    /// The answer to a control request either side sent.
    pub const CONTROL_RESPONSE: &str = "control_response";

    /// `system` subtype: what the session is, sent once per turn.
    pub const INIT: &str = "init";
    /// `result` subtype: the turn finished normally.
    pub const SUCCESS: &str = "success";
    /// `control_request` subtype we send: stop the turn in flight.
    pub const INTERRUPT: &str = "interrupt";
    /// `control_request` subtype the CLI sends when it wants permission for a tool.
    pub const CAN_USE_TOOL: &str = "can_use_tool";
}

/// The flags that select this protocol, verified by running them.
///
/// `--verbose` is not decoration: `--output-format stream-json` refuses to stream the
/// intermediate messages without it, and the intermediate messages are the transcript.
/// `-p` is required by both format flags — their help text says *"only works with --print"*.
const FLAGS: [&str; 6] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
];

/// How many lines of the child's stderr are kept to explain an exit.
///
/// A **bounded tail** rather than a log: without the bound a chatty child left running
/// overnight would grow Velm's memory by everything it ever printed.
const STDERR_TAIL: usize = 40;

// ---------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------

/// What the CLI told us about itself on an `init` line.
///
/// Recorded rather than emitted: it arrives once per turn (measured), so an event would put a
/// line in the transcript every time the user asked anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// The model the CLI resolved for this session — `"claude-opus-5[1m]"` in the capture.
    /// This is *the answer* to "which model is this node on", and it is better than the
    /// node's own guess because the CLI applies the user's own configuration.
    pub model: Option<String>,
    /// `interrupt_receipt_v1`, `interrupt_cancel_queued_v1`, `msg_lifecycle_v1`… Recorded and
    /// deliberately **not** gated on: the interrupt is answered whether or not it is
    /// advertised, so reading this to decide would be a way to break cancel on a version that
    /// spelled its capabilities differently.
    pub capabilities: Vec<String>,
    /// Where the CLI got its credentials. See the module header — nothing acts on it yet.
    pub api_key_source: Option<String>,
    /// The CLI's own version, for a bug report that would otherwise say "it broke".
    pub version: Option<String>,
}

/// What one line from the CLI was.
///
/// Kept as a value rather than acted on inside the decoder so that *"given this exact JSON
/// line, these exact events come out"* is an ordinary offline test. That is the part of this
/// module most likely to be wrong, and the only part that can be checked without spending the
/// user's subscription.
#[derive(Debug, PartialEq)]
enum Line {
    /// Transcript events, in order. Empty is normal — an assistant message whose only block
    /// is `redacted_thinking` says nothing a transcript can show.
    Events(Vec<TranscriptEvent>),
    /// A `result` line: the turn is over.
    Ended(TurnOutcome),
    /// An `init` line: facts about the session, no events.
    Learned(Facts),
    /// The CLI is blocked waiting for us. **Must be answered**, even if only with an error.
    Control { id: String, subtype: String, request: Value },
    /// An unknown type, an unknown subtype, a `control_response`, or not JSON at all.
    Ignore,
}

/// Reads one line of the protocol.
fn read_line(line: &str, blobs: &Blobs) -> Line {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return Line::Ignore;
    };

    match value["type"].as_str().unwrap_or_default() {
        wire::ASSISTANT => Line::Events(assistant_events(&value["message"], blobs)),
        wire::USER => Line::Events(user_events(&value["message"], blobs)),
        wire::RESULT => Line::Ended(map_result(&value)),
        wire::SYSTEM => match value["subtype"].as_str().unwrap_or_default() {
            wire::INIT => Line::Learned(facts(&value)),
            // `hook_started`, `hook_response`, `thinking_tokens` — all measured, none of them
            // the agent talking to the user — and whatever a later version adds.
            _ => Line::Ignore,
        },
        wire::RATE_LIMIT => rate_limit(&value),
        wire::CONTROL_REQUEST => Line::Control {
            id: value["request_id"].as_str().unwrap_or_default().to_owned(),
            subtype: value["request"]["subtype"].as_str().unwrap_or_default().to_owned(),
            request: value["request"].clone(),
        },
        // A `control_response` is the answer to something we sent. There is nothing to
        // correlate: an interrupt's real signal is the `result` line that follows it, and a
        // transport that waited for the receipt would hang on a version that stopped sending
        // one. Ignored on purpose rather than by omission.
        wire::CONTROL_RESPONSE => Line::Ignore,
        _ => Line::Ignore,
    }
}

/// The content blocks of a message, whether one was sent or several.
fn blocks(content: &Value) -> Vec<&Value> {
    match content.as_array() {
        Some(list) => list.iter().collect(),
        None if content.is_object() => vec![content],
        None => Vec::new(),
    }
}

/// The readable text of a content list.
///
/// ⚠ **The content of a `tool_result` block may be a bare string**, not a list — that is
/// Anthropic's message shape, and a decoder that only walks arrays reports every such tool as
/// having answered nothing. Blocks are joined with a newline rather than concatenated: two
/// text blocks run together read as one sentence that was never said.
fn text_of(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    let mut parts = Vec::new();
    for block in blocks(content) {
        if let Some(text) = block["text"].as_str()
            && !text.is_empty()
        {
            parts.push(text);
        }
    }
    parts.join("\n")
}

/// A picture, parked as bytes for the app to write.
///
/// The block is Anthropic's, so the bytes are **nested under `source`** — not ACP's flat
/// `data`/`mimeType`, which is the shape [`super::acp`] handles and the reason this is not
/// shared code. A `source.type` that is not `base64` (a URL, say) is skipped: there is
/// nothing to park, and inventing a fetch here would put the network in a decoder.
fn image_event(block: &Value, blobs: &Blobs) -> Option<TranscriptEvent> {
    let source = if block["source"].is_object() { &block["source"] } else { block };
    if source["type"].as_str().is_some_and(|kind| kind != "base64") {
        return None;
    }
    let mime = source["media_type"]
        .as_str()
        .or_else(|| source["mimeType"].as_str())
        .unwrap_or("image/png");
    let bytes = decode_base64(source["data"].as_str()?)?;
    // Parked **before** the event is built, so a caller that drains blobs first can never see
    // a placeholder it has no bytes for. See `transport::PendingBlob`.
    let blob = park_blob(blobs, mime, bytes);
    Some(TranscriptEvent::Image { blob, caption: block["caption"].as_str().map(str::to_owned) })
}

/// The events in one `assistant` message.
fn assistant_events(message: &Value, blobs: &Blobs) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    for block in blocks(&message["content"]) {
        match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let text = block["text"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    events.push(TranscriptEvent::Text { text: text.to_owned() });
                }
            }
            "thinking" => {
                let text = block["thinking"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    events.push(TranscriptEvent::Thought { text: text.to_owned() });
                }
            }
            // A redacted thinking block is ciphertext by design: there is nothing to show, and
            // a placeholder line would report the agent as having thought something visible.
            "redacted_thinking" => {}
            "tool_use" => events.push(TranscriptEvent::ToolCall {
                id: ToolCallId(block["id"].as_str().unwrap_or_default().to_owned()),
                name: block["name"].as_str().unwrap_or("a tool").to_owned(),
                input: match &block["input"] {
                    Value::Null => String::new(),
                    input => input.to_string(),
                },
            }),
            "image" => events.extend(image_event(block, blobs)),
            _ => {}
        }
    }
    events
}

/// The events in one `user` message.
///
/// Only `tool_result` blocks are mined. Everything else on a `user` line is either our own
/// prompt echoed back (`--replay-user-messages`, which we do not pass) or the CLI narrating —
/// *"[Request interrupted by user]"* arrives exactly this way, measured, and putting it in the
/// transcript as the *user* having said it would be a fabrication.
fn user_events(message: &Value, blobs: &Blobs) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    for block in blocks(&message["content"]) {
        if block["type"].as_str().unwrap_or_default() != "tool_result" {
            continue;
        }
        events.push(TranscriptEvent::ToolResult {
            id: ToolCallId(block["tool_use_id"].as_str().unwrap_or_default().to_owned()),
            output: text_of(&block["content"]),
            ok: !block["is_error"].as_bool().unwrap_or(false),
        });
        // A tool can answer with a picture — a screenshot, a chart it drew. This is the case
        // `PendingBlob` exists for in practice; the model itself does not send images.
        for inner in blocks(&block["content"]) {
            if inner["type"].as_str() == Some("image") {
                events.extend(image_event(inner, blobs));
            }
        }
    }
    events
}

/// How a turn stopped, from the `result` line.
///
/// **`success` with `is_error: true` is a failure**, and that pair is not hypothetical: an
/// interrupted turn was measured emitting `"is_error": true`, so trusting the subtype alone
/// would report a stopped turn as a finished one.
///
/// The non-success subtypes are **recalled from the Agent SDK's protocol, not measured** —
/// `error_max_turns`, `error_during_execution`. So the rule is written the other way round
/// from [`super::acp::map_stop_reason`]: anything unrecognised is a **failure naming the
/// subtype**, never a silent success. A transcript that says *"stopped: error_something_new"*
/// is honest and actionable; one that says "finished" when it did not is neither.
fn map_result(value: &Value) -> TurnOutcome {
    let subtype = value["subtype"].as_str().unwrap_or_default();
    let is_error = value["is_error"].as_bool().unwrap_or(false);

    if subtype == wire::SUCCESS && !is_error {
        return TurnOutcome::Completed;
    }

    let detail = value["result"]
        .as_str()
        .or_else(|| value["error"].as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let named = if subtype.is_empty() {
        "the agent gave no reason".to_owned()
    } else {
        subtype.to_owned()
    };
    let message = match detail {
        Some(detail) => format!("{named}: {detail}"),
        None => named,
    };

    // A budget is a limit the run reached, not a fault: the transcript says *"ran out"* and
    // the node offers to continue, where a failure says *"fix this first"*.
    if subtype.contains("max_turns")
        || subtype.contains("budget")
        || subtype.contains("max_tokens")
        || subtype.contains("rate_limit")
    {
        return TurnOutcome::Exhausted { message };
    }
    TurnOutcome::Failed { message }
}

/// What an `init` line says about the session.
fn facts(value: &Value) -> Facts {
    Facts {
        model: value["model"].as_str().map(str::to_owned),
        capabilities: value["capabilities"]
            .as_array()
            .map(|list| list.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
        api_key_source: value["apiKeySource"].as_str().map(str::to_owned),
        version: value["claude_code_version"].as_str().map(str::to_owned),
    }
}

/// The subscription's rate-limit window.
///
/// Silent while it is `allowed`, which is every line in the capture. It is worth an event only
/// when it is *not*, because that is the one moment the user needs to know their subscription
/// — not their code — is what stopped the agent.
fn rate_limit(value: &Value) -> Line {
    let info = &value["rate_limit_info"];
    let status = info["status"].as_str().unwrap_or("allowed");
    if status == "allowed" {
        return Line::Ignore;
    }
    let window = info["rateLimitType"].as_str().unwrap_or("usage");
    Line::Events(vec![TranscriptEvent::Error {
        message: format!("your Claude subscription's {window} limit is {status}"),
    }])
}

/// One line of input: a turn.
fn user_message(prompt: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": prompt }] },
    })
}

// ---------------------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------------------

/// A tool the CLI is blocked on, waiting for the user's answer.
struct Pending {
    /// The CLI's own `request_id`, echoed back verbatim.
    control_id: String,
    /// The tool's input, echoed back on an allow. See [`ClaudeCli::answer_permission`].
    input: Value,
}

/// Everything the reader thread and the caller both touch.
struct Shared {
    stdin: Mutex<Option<ChildStdin>>,
    events: Sender<TranscriptEvent>,
    blobs: Blobs,
    /// Whether the child is still there. `true` before one is spawned — nothing has died.
    alive: AtomicBool,
    /// Set by [`AgentTransport::cancel`], cleared by every prompt. What turns the `result`
    /// line an interrupt produces into `Cancelled` rather than the failure it looks like.
    cancelled: AtomicBool,
    /// The turn in flight. `take`n by whichever of the reader and `shutdown` gets there
    /// first, which is what makes exactly one `TurnEnded` per `TurnStarted` true even when
    /// the process dies mid-turn.
    turn: Mutex<Option<TurnId>>,
    facts: Mutex<Facts>,
    permissions: Mutex<HashMap<String, Pending>>,
    stderr: Mutex<VecDeque<String>>,
    next_control: AtomicU64,
}

impl Shared {
    fn new(events: Sender<TranscriptEvent>) -> Self {
        Self {
            stdin: Mutex::new(None),
            events,
            blobs: Blobs::default(),
            alive: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            turn: Mutex::new(None),
            facts: Mutex::new(Facts::default()),
            permissions: Mutex::new(HashMap::new()),
            stderr: Mutex::new(VecDeque::new()),
            next_control: AtomicU64::new(1),
        }
    }

    fn write_line(&self, message: &Value) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let stdin = guard.as_mut().ok_or_else(|| AgentError::Transport {
            transport: "claude",
            message: "the agent's input has been closed".into(),
        })?;
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| AgentError::Transport {
                transport: "claude",
                message: error.to_string(),
            })
    }

    fn emit(&self, event: TranscriptEvent) {
        let _ = self.events.send(event);
    }

    fn control_id(&self) -> String {
        format!("velm-{}", self.next_control.fetch_add(1, Ordering::Relaxed))
    }

    /// Ends the turn in flight, if there is one. Idempotent by construction: the id is taken.
    fn end_turn(&self, outcome: TurnOutcome) {
        let turn = self.turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(turn) = turn {
            let outcome = if self.cancelled.swap(false, Ordering::Relaxed) {
                TurnOutcome::Cancelled
            } else {
                outcome
            };
            self.emit(TranscriptEvent::TurnEnded { turn, outcome });
        }
    }

    fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// What was resolved at [`ClaudeCli::start`] and is needed at the spawn.
struct Launch {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

/// A `claude` process, speaking its own stream-json protocol.
pub struct ClaudeCli {
    shared: Arc<Shared>,
    launch: Launch,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    errors: Option<JoinHandle<()>>,
    shut_down: bool,
}

impl std::fmt::Debug for ClaudeCli {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeCli")
            .field("program", &self.launch.program)
            .field("started", &self.child.is_some())
            .field("alive", &self.shared.alive.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ClaudeCli {
    /// Resolves the command and the arguments. **Starts no process.**
    ///
    /// The binary is probed here rather than at the spawn, so a machine without `claude` gets
    /// *"`claude` is not installed or is not on your PATH"* the moment the node is started
    /// rather than at the first prompt. The *spawn* is deferred to that first prompt for a
    /// reason the capture shows outright: starting the child fires the user's own
    /// `SessionStart` hooks, so an eager spawn would run them once per agent node placed on a
    /// board, for agents nobody has asked anything.
    pub fn start(spec: &LaunchSpec, events: Sender<TranscriptEvent>) -> Result<Self> {
        let command = spec.resolved_command().filter(|name| !name.is_empty()).ok_or_else(|| {
            AgentError::Refused(format!(
                "{} has no agent command to delegate to — it can only be reached over its API",
                spec.provider.provider.label()
            ))
        })?;
        let program = probe_command(command)?;

        let mut args: Vec<String> = FLAGS.iter().map(|flag| (*flag).to_owned()).collect();

        // `--append-system-prompt`, never `--system-prompt`: the latter *replaces* Claude
        // Code's own prompt, which is where its tool instructions live — a node given a role
        // would silently lose the ability to use its tools properly. Velm's resolved context
        // is an addition to who the agent is, not a replacement for it.
        if !spec.system_context.trim().is_empty() {
            args.push("--append-system-prompt".to_owned());
            args.push(spec.system_context.clone());
        }
        // Only when the node named one. No model id is pinned anywhere in this crate: with no
        // flag the CLI uses whatever the user's own configuration considers current, which is
        // more likely to be right than anything written here.
        if let Some(model) = spec.provider.model.as_deref().filter(|name| !name.is_empty()) {
            args.push("--model".to_owned());
            args.push(model.to_owned());
        }
        args.extend(spec.args.iter().cloned());

        let cwd = spec.cwd.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();

        Ok(Self {
            shared: Arc::new(Shared::new(events)),
            launch: Launch { program, args, cwd, env: spec.env.clone() },
            child: None,
            reader: None,
            errors: None,
            shut_down: false,
        })
    }

    /// What the CLI said about itself on the last `init` line. Empty until the first turn.
    pub fn facts(&self) -> Facts {
        self.shared.facts.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Spawns the child, once.
    fn ensure_started(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }

        let mut process = Command::new(&self.launch.program);
        process
            .args(&self.launch.args)
            .current_dir(&self.launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.launch.env {
            process.env(key, value);
        }
        // Note what is *not* here: no `ANTHROPIC_API_KEY`, set or cleared. See the header.

        let mut child = process.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AgentError::MissingCommand {
                    command: self.launch.program.display().to_string(),
                }
            } else {
                AgentError::Transport { transport: "claude", message: error.to_string() }
            }
        })?;

        *self.shared.stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            child.stdin.take();

        if let Some(stdout) = child.stdout.take() {
            let shared = Arc::clone(&self.shared);
            self.reader = Some(
                std::thread::Builder::new()
                    .name("velm-claude".into())
                    .spawn(move || pump(BufReader::new(stdout), &shared))
                    .map_err(AgentError::Io)?,
            );
        }
        if let Some(stderr) = child.stderr.take() {
            let shared = Arc::clone(&self.shared);
            self.errors = Some(
                std::thread::Builder::new()
                    .name("velm-claude-log".into())
                    .spawn(move || {
                        for line in
                            BufReader::new(stderr).lines().map_while(std::result::Result::ok)
                        {
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
                    .map_err(AgentError::Io)?,
            );
        }

        self.child = Some(child);
        Ok(())
    }
}

impl AgentTransport for ClaudeCli {
    fn kind(&self) -> TransportKind {
        TransportKind::ClaudeCli
    }

    /// Sends one turn. **Returns immediately**; everything else arrives on the reader thread.
    ///
    /// The prompt is written on a **detached one-shot thread** rather than here, because
    /// `AgentTransport`'s contract is that no method blocks the caller and the caller is the
    /// frame loop: a prompt longer than the pipe's buffer would block on `write_all` until the
    /// child read it. There is no thread to join afterwards — the answer is not the write's,
    /// it is the reader's, and a write that fails ends the turn from inside the closure so a
    /// prompt that never left cannot leave a turn open forever.
    fn send_prompt(&mut self, turn: TurnId, prompt: &str) -> Result<()> {
        if self.shared.turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
            return Err(AgentError::Refused(
                "this agent is still working — the prompt was not sent".into(),
            ));
        }
        self.ensure_started()?;
        if !self.shared.alive.load(Ordering::Relaxed) {
            return Err(AgentError::Transport {
                transport: "claude",
                message: "the agent process has ended".into(),
            });
        }

        self.shared.cancelled.store(false, Ordering::Relaxed);
        *self.shared.turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn);
        self.shared.emit(TranscriptEvent::TurnStarted { turn, prompt: prompt.to_owned() });

        let shared = Arc::clone(&self.shared);
        let line = user_message(prompt);
        std::thread::Builder::new()
            .name("velm-claude-prompt".into())
            .spawn(move || {
                if let Err(error) = shared.write_line(&line) {
                    shared.end_turn(TurnOutcome::Failed { message: error.to_string() });
                }
            })
            .map_err(AgentError::Io)?;
        Ok(())
    }

    /// Stops the turn in flight, without killing the child.
    ///
    /// Measured (2026-08-13): the CLI answers the interrupt, emits a `user` line reading
    /// *"[Request interrupted by user]"*, ends the turn with a `result` carrying
    /// `"is_error": true`, and **stays alive to take the next prompt**. Killing the child
    /// would work and would throw away the conversation — the second turn of a session reads
    /// the first out of the prompt cache, so a cancel that killed would silently make every
    /// following turn start from nothing.
    ///
    /// The flag is what makes the outcome `Cancelled`: the `result` line an interrupt produces
    /// is indistinguishable from a failure, and reporting the user's own stop as a red error
    /// on the node is the one wrong answer here.
    fn cancel(&mut self) -> Result<()> {
        let in_flight =
            self.shared.turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some();
        if !in_flight || self.child.is_none() {
            return Ok(());
        }
        self.shared.cancelled.store(true, Ordering::Relaxed);
        self.shared.write_line(&json!({
            "type": "control_request",
            "request_id": self.shared.control_id(),
            "request": { "subtype": wire::INTERRUPT },
        }))
    }

    /// Answers a tool-permission request.
    ///
    /// ⚠ **The response shape here is recalled from the Agent SDK's control protocol, not
    /// measured**, and the request that would exercise it is dormant: Velm passes no
    /// `--permission-prompt-tool`, so the CLI applies the user's own permission settings and
    /// has never been observed asking. It is implemented anyway because [`act`] must answer
    /// *every* control request — one left unanswered blocks the CLI forever — and answering a
    /// permission question with "not implemented" would be a refusal the user never made.
    fn answer_permission(&mut self, id: &RequestId, allowed: bool) -> Result<()> {
        let pending = self
            .shared
            .permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id.0);
        let Some(pending) = pending else {
            return Ok(());
        };
        let answer = if allowed {
            // The input is echoed back because the protocol allows the client to *amend* what
            // the tool is called with; sending it back unchanged is the "yes, as asked" case.
            json!({ "behavior": "allow", "updatedInput": pending.input })
        } else {
            json!({ "behavior": "deny", "message": "the user did not allow this" })
        };
        let written = self.shared.write_line(&json!({
            "type": "control_response",
            "response": { "subtype": wire::SUCCESS, "request_id": pending.control_id, "response": answer },
        }));
        // Emitted here rather than by the caller, so the answer lands in the one ordered
        // stream the sidecar records — see `AgentTransport::answer_permission`.
        self.shared.emit(TranscriptEvent::PermissionAnswer { id: id.clone(), allowed });
        written
    }

    fn take_pending_blobs(&mut self) -> Vec<PendingBlob> {
        std::mem::take(
            &mut *self.shared.blobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// Ends the session and releases the process. Idempotent.
    fn shutdown(&mut self) -> Result<()> {
        if self.shut_down {
            return Ok(());
        }
        self.shut_down = true;
        // Set **before** the kill, because the reader is what ends a turn that was in flight
        // and it is about to see the pipe close. Closing a node mid-turn is the user stopping
        // it; without this the transcript would record the shutdown they asked for as a
        // failure of the agent.
        self.shared.cancelled.store(true, Ordering::Relaxed);

        // Anything the CLI is blocked on is refused first: it is waiting for a reply on a pipe
        // we are about to close, and a child left waiting on a closed pipe is how one ends up
        // unkillable on some platforms.
        let outstanding: Vec<String> = self
            .shared
            .permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for id in outstanding {
            let _ = self.answer_permission(&RequestId(id), false);
        }

        // Closing stdin is the polite request to exit; the kill is what makes shutdown
        // bounded when the CLI is mid-turn and not reading.
        drop(self.shared.stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take());

        let killed = match self.child.as_mut() {
            Some(child) => {
                let killed = child.kill();
                let _ = child.wait();
                killed
            }
            None => Ok(()),
        };
        self.shared.alive.store(false, Ordering::Relaxed);

        // Joined, not detached: the reader is what ends a turn that was in flight, and a
        // shutdown that returned before that event was sent would drop it on the floor.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(errors) = self.errors.take() {
            let _ = errors.join();
        }
        // Belt and braces for the case where there was no reader at all — a child that failed
        // to give us its stdout still owes the session a `TurnEnded`. A no-op when the reader
        // has already taken the turn, because the id is taken rather than read.
        self.shared.end_turn(TurnOutcome::Cancelled);

        killed.map_err(|error| AgentError::Transport {
            transport: "claude",
            message: error.to_string(),
        })
    }
}

impl Drop for ClaudeCli {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The reader thread: one JSON object per line, for the life of the process.
fn pump(reader: impl BufRead, shared: &Shared) {
    for line in reader.lines().map_while(std::result::Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        act(read_line(&line, &shared.blobs), shared);
    }

    shared.alive.store(false, Ordering::Relaxed);
    let tail = shared.stderr_tail();

    // A turn still in flight when the pipe closed never got its `result`, and a turn with no
    // end is a node that spins forever.
    let in_flight =
        shared.turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some();
    if in_flight {
        let message = if tail.is_empty() {
            "the agent process ended before it answered".to_owned()
        } else {
            format!("the agent process ended: {tail}")
        };
        shared.end_turn(TurnOutcome::Failed { message });
    } else if !tail.is_empty() {
        // Only worth saying when the CLI left something behind: a clean exit after the user
        // closed the node is not an error, and reporting one would put a red line on every
        // node that was ever shut down.
        shared.emit(TranscriptEvent::Error {
            message: format!("the agent process ended: {tail}"),
        });
    }
}

/// What to do with one decoded line.
fn act(line: Line, shared: &Shared) {
    match line {
        Line::Events(events) => {
            for event in events {
                shared.emit(event);
            }
        }
        Line::Ended(outcome) => shared.end_turn(outcome),
        Line::Learned(facts) => {
            *shared.facts.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = facts;
        }
        Line::Control { id, subtype, request } if subtype == wire::CAN_USE_TOOL => {
            let tool = request["tool_name"].as_str().unwrap_or("a tool").to_owned();
            let detail = match &request["input"] {
                Value::Null => String::new(),
                input => input.to_string(),
            };
            let asked = RequestId(format!("claude-{id}"));
            shared
                .permissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(asked.0.clone(), Pending { control_id: id, input: request["input"].clone() });
            shared.emit(TranscriptEvent::PermissionRequest {
                id: asked,
                summary: format!("run {tool}"),
                detail,
            });
        }
        Line::Control { id, subtype, .. } => {
            // ⚠ A control request **must** be answered, even one we do not implement: the CLI
            // is blocked on it, and silence is a hang rather than a refusal.
            let _ = shared.write_line(&json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": id,
                    "error": format!("Velm does not implement the `{subtype}` control request"),
                },
            }));
        }
        Line::Ignore => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc::{Receiver, channel};

    /// The literals below were **taken from real sessions captured on this machine on
    /// 2026-08-13**, with session ids redacted — *except where a constant's own comment says a
    /// field was reconstructed*, which two of them do. That distinction is the whole point:
    /// the mapping is the part of this module that cannot be checked by running the CLI in CI,
    /// and a fixture invented from the same memory that wrote the parser proves nothing. A
    /// constant called `CAPTURED` that was not is worse than no fixture at all, because the
    /// next reader will treat it as wire evidence.
    const CAPTURED_ASSISTANT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5","id":"msg_011CdzbgqH5Dgpbpau5PLw8R","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":30629,"cache_read_input_tokens":0,"output_tokens":4,"service_tier":"standard"},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"<redacted>","uuid":"<redacted>","timestamp":"2026-08-13T10:35:16.014Z"}"#;

    const CAPTURED_RESULT: &str = r#"{"is_error":false,"duration_api_ms":2117,"num_turns":1,"stop_reason":"end_turn","session_id":"<redacted>","total_cost_usd":0.3064,"usage":{"input_tokens":2,"output_tokens":4},"permission_denials":[],"terminal_reason":"completed","subtype":"success","api_error_status":null,"result":"ok","type":"result","duration_ms":5086,"uuid":"<redacted>"}"#;

    /// ⚠ **Partly reconstructed.** `type`, `subtype`, `cwd`, `session_id`, `tools`,
    /// `mcp_servers`, `model` and `permissionMode` are verbatim from the capture; the capture
    /// window ended inside `slash_commands`, so **`capabilities`, `apiKeySource` and
    /// `claude_code_version` were never seen** — their spellings come from the brief that
    /// commissioned this module. That is exactly why [`facts`] reads all three defensively and
    /// why nothing in this transport is gated on `capabilities`: if a spelling is wrong, the
    /// field is `None` and the interrupt still works.
    const INIT_LINE: &str = r#"{"type":"system","subtype":"init","cwd":"/private/tmp","session_id":"<redacted>","tools":["Bash","Read"],"mcp_servers":[],"model":"claude-opus-5[1m]","permissionMode":"plan","capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","msg_lifecycle_v1"],"apiKeySource":"none","claude_code_version":"2.0.0"}"#;

    /// The `result` line a **real interrupted turn** produced. Everything up to and including
    /// `total_cost_usd` is verbatim, and the pair that matters — `"is_error": true` against a
    /// `stop_reason` of `null` — is measured: this is the line that must not be reported as a
    /// finished answer.
    ///
    /// ⚠ **`subtype` is reconstructed.** The probe's own output filter cut the line at 300
    /// characters, before it; `error_during_execution` is the Agent SDK's name recalled, not
    /// read. Nothing here depends on *which* error it is — with the cancel flag set the
    /// outcome is `Cancelled`, and without it any unrecognised subtype is a named failure.
    /// The one thing a wrong guess would change is the second half of
    /// `an_interrupted_turn_is_cancelled_rather_than_failed`: a real subtype containing
    /// "max" or "budget" would make the un-flagged case `Exhausted` rather than `Failed`.
    const INTERRUPTED_RESULT: &str = r#"{"is_error":true,"duration_api_ms":795,"num_turns":2,"stop_reason":null,"session_id":"<redacted>","total_cost_usd":0.0006180000000000001,"subtype":"error_during_execution","type":"result","uuid":"<redacted>"}"#;

    /// Measured: a `system` subtype this module has never heard of, arriving mid-turn.
    const CAPTURED_THINKING_TOKENS: &str = r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":150,"estimated_tokens_delta":100,"uuid":"<redacted>","session_id":"<redacted>"}"#;

    fn events_of(line: &str) -> Vec<TranscriptEvent> {
        match read_line(line, &Blobs::default()) {
            Line::Events(events) => events,
            other => panic!("expected events, got {other:?}"),
        }
    }

    /// A `Shared` with no child behind it, so the reader thread's own logic is testable
    /// offline. `stdin: None` is the honest state before a spawn, and every write through it
    /// fails — which is what the permission tests rely on to stay hermetic.
    fn detached() -> (Arc<Shared>, Receiver<TranscriptEvent>) {
        let (sender, receiver) = channel();
        (Arc::new(Shared::new(sender)), receiver)
    }

    #[test]
    fn the_captured_assistant_line_is_one_text_event() {
        assert_eq!(
            events_of(CAPTURED_ASSISTANT),
            vec![TranscriptEvent::Text { text: "ok".into() }]
        );
    }

    #[test]
    fn the_captured_result_line_ends_the_turn() {
        assert_eq!(read_line(CAPTURED_RESULT, &Blobs::default()), Line::Ended(TurnOutcome::Completed));
    }

    /// `init` is a fact, not an event — and it arrives **once per turn** (measured), so a
    /// version of this that emitted anything would put a line in the transcript every time the
    /// user asked a question.
    #[test]
    fn init_teaches_the_model_and_says_nothing_even_when_it_repeats() {
        let Line::Learned(facts) = read_line(INIT_LINE, &Blobs::default()) else {
            panic!("init did not decode as facts");
        };
        assert_eq!(facts.model.as_deref(), Some("claude-opus-5[1m]"));
        assert!(facts.capabilities.iter().any(|name| name == "interrupt_receipt_v1"));
        assert_eq!(facts.version.as_deref(), Some("2.0.0"));

        let (shared, events) = detached();
        act(read_line(INIT_LINE, &shared.blobs), &shared);
        act(read_line(INIT_LINE, &shared.blobs), &shared);
        assert!(events.try_recv().is_err(), "an init line reached the transcript");
        assert_eq!(shared.facts.lock().unwrap().model.as_deref(), Some("claude-opus-5[1m]"));
    }

    /// The CLI ships on its own schedule. A line this module has never seen is a **newer
    /// version**, not a broken session — and a line that is not JSON at all is a partial write
    /// or a stray print, neither of which is worth ending a conversation over.
    #[test]
    fn an_unknown_line_is_ignored_rather_than_fatal() {
        for line in [
            CAPTURED_THINKING_TOKENS,
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"something_a_later_version_invented","payload":{}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"velm-1"}}"#,
            "{ not json at all",
            "",
            "null",
        ] {
            assert_eq!(read_line(line, &Blobs::default()), Line::Ignore, "{line}");
        }
    }

    /// The required negative: an unknown `subtype` must **name itself in a failure**, never
    /// pass as success. `acp.rs` takes the opposite default — an unknown stop reason there is
    /// a finished answer — and the difference is deliberate: this protocol spells its errors
    /// in the subtype, so "unrecognised" here means "something went wrong that we cannot name",
    /// which is exactly what the transcript should say.
    #[test]
    fn a_result_with_an_unknown_subtype_fails_and_names_it() {
        let line = r#"{"type":"result","subtype":"error_from_the_future","is_error":true,"result":"the sky fell"}"#;
        let Line::Ended(TurnOutcome::Failed { message }) = read_line(line, &Blobs::default())
        else {
            panic!("an unknown subtype did not fail");
        };
        assert!(message.contains("error_from_the_future"), "{message}");
        assert!(message.contains("the sky fell"), "{message}");

        // ⚠ `success` **with** `is_error` is a failure. Measured on an interrupted turn, so
        // trusting the subtype alone would report a stopped turn as a finished one.
        let lying = r#"{"type":"result","subtype":"success","is_error":true,"result":"nope"}"#;
        assert!(matches!(
            read_line(lying, &Blobs::default()),
            Line::Ended(TurnOutcome::Failed { .. })
        ));

        // A budget is a limit reached, not a fault: *"ran out"* rather than *"fix this"*.
        let spent = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        let Line::Ended(TurnOutcome::Exhausted { message }) = read_line(spent, &Blobs::default())
        else {
            panic!("a budget was reported as a failure");
        };
        assert!(message.contains("error_max_turns"), "{message}");

        // A result with no subtype at all still has to say something a person can act on.
        let mute = r#"{"type":"result","is_error":true}"#;
        let Line::Ended(TurnOutcome::Failed { message }) = read_line(mute, &Blobs::default())
        else {
            panic!("a subtypeless result did not fail");
        };
        assert!(!message.is_empty());
    }

    /// The user's own stop is not a failure, and the `result` line an interrupt produces is
    /// indistinguishable from one. The flag is the whole difference.
    #[test]
    fn an_interrupted_turn_is_cancelled_rather_than_failed() {
        let (shared, events) = detached();
        *shared.turn.lock().unwrap() = Some(TurnId(7));
        shared.cancelled.store(true, Ordering::Relaxed);
        act(read_line(INTERRUPTED_RESULT, &shared.blobs), &shared);
        assert_eq!(
            events.try_recv().unwrap(),
            TranscriptEvent::TurnEnded { turn: TurnId(7), outcome: TurnOutcome::Cancelled }
        );

        // Without the flag the same line is the failure it looks like — and the flag does not
        // survive into the next turn, or every turn after a cancel would be reported stopped.
        let (shared, events) = detached();
        *shared.turn.lock().unwrap() = Some(TurnId(8));
        act(read_line(INTERRUPTED_RESULT, &shared.blobs), &shared);
        assert!(matches!(
            events.try_recv().unwrap(),
            TranscriptEvent::TurnEnded { turn: TurnId(8), outcome: TurnOutcome::Failed { .. } }
        ));
        assert!(!shared.cancelled.load(Ordering::Relaxed));
    }

    /// A tool call and its answer, in Anthropic's own shapes. The **string** content is the
    /// trap: `tool_result.content` is an array in some tools and a bare string in others, and
    /// a decoder that only walks arrays reports every such tool as having said nothing.
    #[test]
    fn a_tool_call_and_its_answer_join_up() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert_eq!(
            events_of(call),
            vec![TranscriptEvent::ToolCall {
                id: ToolCallId("toolu_01".into()),
                name: "Bash".into(),
                input: r#"{"command":"ls"}"#.into(),
            }]
        );

        let listed = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"a\nb"}]}}"#;
        assert_eq!(
            events_of(listed),
            vec![TranscriptEvent::ToolResult {
                id: ToolCallId("toolu_01".into()),
                output: "a\nb".into(),
                ok: true,
            }]
        );

        let broken = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_02","is_error":true,"content":[{"type":"text","text":"no such file"}]}]}}"#;
        assert_eq!(
            events_of(broken),
            vec![TranscriptEvent::ToolResult {
                id: ToolCallId("toolu_02".into()),
                output: "no such file".into(),
                ok: false,
            }]
        );
    }

    /// A `user` line is not the user. Measured: an interrupt makes the CLI write one reading
    /// *"[Request interrupted by user]"*, and putting that in the transcript as something the
    /// person said would be a fabrication in the one record that has to be replayable.
    #[test]
    fn a_user_line_that_is_not_a_tool_result_says_nothing() {
        let narration = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"session_id":"<redacted>"}"#;
        assert_eq!(events_of(narration), Vec::<TranscriptEvent>::new());
    }

    /// Thinking is Raw mode's, and a redacted block has nothing to show — a placeholder line
    /// would report the agent as having thought something the user could read.
    #[test]
    fn thinking_becomes_a_thought_and_redacted_thinking_becomes_nothing() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"weighing it up"},{"type":"redacted_thinking","data":"AAAA"},{"type":"text","text":"done"}]}}"#;
        assert_eq!(
            events_of(line),
            vec![
                TranscriptEvent::Thought { text: "weighing it up".into() },
                TranscriptEvent::Text { text: "done".into() },
            ]
        );
    }

    /// The bytes go out through the pending-blob queue; this crate must not write to the blob
    /// store. The shape is **source-nested**, which is where ACP's flat `data`/`mimeType`
    /// decoder would silently produce nothing.
    #[test]
    fn an_image_is_parked_as_bytes_rather_than_written() {
        let blobs = Blobs::default();
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}}]}]}}"#;
        let Line::Events(events) = read_line(line, &blobs) else {
            panic!("the image line produced no events");
        };
        let Some(TranscriptEvent::Image { blob, .. }) = events
            .iter()
            .find(|event| matches!(event, TranscriptEvent::Image { .. }))
            .cloned()
        else {
            panic!("no image event: {events:?}");
        };
        assert!(blob.starts_with("pending:"), "{blob}");

        let parked = blobs.lock().unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].bytes, b"hi");
        assert_eq!(parked[0].mime, "image/png");
        assert_eq!(parked[0].id, blob);

        // A source we cannot park is skipped rather than guessed at: there is nothing to write
        // and fetching a URL from inside a decoder would put the network in the parser.
        let remote = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]}}"#;
        assert_eq!(events_of(remote), Vec::<TranscriptEvent>::new());
    }

    /// Silent while the subscription is fine, which is every line in the capture. It is worth
    /// an event only when it is not — that is the one moment the user needs to know their
    /// *subscription*, rather than their code, is what stopped the agent.
    #[test]
    fn a_rate_limit_is_reported_only_when_it_bites() {
        let allowed = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"}}"#;
        assert_eq!(read_line(allowed, &Blobs::default()), Line::Ignore);

        let blocked = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
        let events = events_of(blocked);
        let [TranscriptEvent::Error { message }] = events.as_slice() else {
            panic!("a rejected rate limit said nothing: {events:?}");
        };
        assert!(message.contains("five_hour"), "{message}");
        assert!(message.contains("subscription"), "{message}");
    }

    /// The required negative, at the level that matters: a line the reader cannot parse must
    /// not stop the reader. A session that ends on a stray print is a node that goes quiet
    /// halfway through an answer and never says why.
    #[test]
    fn a_broken_line_does_not_end_the_session() {
        let (shared, events) = detached();
        *shared.turn.lock().unwrap() = Some(TurnId(3));
        let stream = format!(
            "{INIT_LINE}\n{{ half a line\n\n{CAPTURED_THINKING_TOKENS}\n{CAPTURED_ASSISTANT}\n{CAPTURED_RESULT}\n"
        );
        pump(Cursor::new(stream), &shared);

        let seen: Vec<TranscriptEvent> = events.try_iter().collect();
        assert_eq!(
            seen,
            vec![
                TranscriptEvent::Text { text: "ok".into() },
                TranscriptEvent::TurnEnded { turn: TurnId(3), outcome: TurnOutcome::Completed },
            ],
            "the reader did not survive the broken line"
        );
        assert!(!shared.alive.load(Ordering::Relaxed), "the reader ended without saying so");
    }

    /// Exactly one `TurnEnded` per `TurnStarted`, even when the process dies mid-turn. A turn
    /// with no end is a node that spins forever, and the stderr tail is the only thing that
    /// can say why — `claude` writes its own failures there, not to the protocol.
    #[test]
    fn a_turn_that_outlives_its_process_still_ends_and_says_why() {
        let (shared, events) = detached();
        *shared.turn.lock().unwrap() = Some(TurnId(11));
        shared.stderr.lock().unwrap().push_back("Error: not logged in".to_owned());
        pump(Cursor::new(String::new()), &shared);

        let TranscriptEvent::TurnEnded { turn, outcome: TurnOutcome::Failed { message } } =
            events.try_recv().unwrap()
        else {
            panic!("a turn outlived its process without ending");
        };
        assert_eq!(turn, TurnId(11));
        assert!(message.contains("not logged in"), "{message}");

        // With no turn in flight, a clean exit is not an error — a red line on every node that
        // was ever shut down is worse than silence.
        let (shared, events) = detached();
        pump(Cursor::new(String::new()), &shared);
        assert!(events.try_recv().is_err());
    }

    /// A `result` arriving with nothing in flight cannot invent a turn to end. It happens on
    /// the line after a `shutdown` took the turn, and a `TurnId::default()` fabricated here
    /// would write an event the sidecar can never join to a prompt.
    #[test]
    fn a_result_with_no_turn_in_flight_is_dropped() {
        let (shared, events) = detached();
        act(read_line(CAPTURED_RESULT, &shared.blobs), &shared);
        assert!(events.try_recv().is_err());
    }

    /// ⚠ A control request **must** be answered — the CLI is blocked on it, and silence is a
    /// hang rather than a refusal. A `can_use_tool` becomes the question only the user can
    /// answer; anything else is answered with an error, which is a reply.
    #[test]
    fn a_permission_request_becomes_a_question_and_the_rest_are_answered() {
        let asking = r#"{"type":"control_request","request_id":"req_9","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"}}}"#;
        let Line::Control { id, subtype, .. } = read_line(asking, &Blobs::default()) else {
            panic!("a control request did not decode as one");
        };
        assert_eq!(id, "req_9");
        assert_eq!(subtype, wire::CAN_USE_TOOL);

        let (shared, events) = detached();
        act(read_line(asking, &shared.blobs), &shared);
        let TranscriptEvent::PermissionRequest { id, summary, detail } =
            events.try_recv().unwrap()
        else {
            panic!("a tool permission request was not asked");
        };
        assert_eq!(id.0, "claude-req_9");
        assert!(summary.contains("Bash"), "{summary}");
        assert!(detail.contains("rm -rf /"), "{detail}");
        // Held, so the answer can be routed back to the id the CLI is blocked on.
        assert!(shared.permissions.lock().unwrap().contains_key("claude-req_9"));

        // An unrecognised control request emits nothing and is answered on the wire. With no
        // child there is nowhere to write, which is exactly the state this asserts is
        // survivable: `act` must not panic or hang on a failed answer.
        let unknown = r#"{"type":"control_request","request_id":"req_10","request":{"subtype":"a_later_version_invented_this"}}"#;
        act(read_line(unknown, &shared.blobs), &shared);
        assert!(events.try_recv().is_err());
    }

    /// The flags are the protocol. They were verified by running exactly this list, and
    /// `--verbose` is the one that looks droppable and is not: without it the stream carries
    /// only the final result, which is a transcript with the work taken out.
    #[test]
    fn the_flags_select_the_protocol_that_was_measured() {
        let (sender, _events) = channel();
        let spec = LaunchSpec::new(crate::provider::ProviderChoice::new(
            crate::provider::Provider::Claude,
        ))
        .with_command("sh") // present on every Unix, and never started by this test
        .with_system_context("You are the Reviewer.");

        let Ok(transport) = ClaudeCli::start(&spec, sender) else {
            return; // no `sh`: nothing to assert about a machine that cannot run one
        };
        let args = &transport.launch.args;
        let expected: Vec<String> = FLAGS.iter().map(|flag| (*flag).to_owned()).collect();
        assert_eq!(args[..FLAGS.len()].to_vec(), expected);
        assert!(args.iter().any(|arg| arg == "--append-system-prompt"));
        assert!(args.iter().any(|arg| arg == "You are the Reviewer."));

        // ⚠ `--bare` disables the OAuth path this feature exists to use. It must never appear.
        assert!(!args.iter().any(|arg| arg == "--bare"), "{args:?}");
        // No model is pinned: with no flag the CLI uses the user's own current model.
        assert!(!args.iter().any(|arg| arg == "--model"), "{args:?}");
        // Nothing was spawned by resolving the command.
        assert!(transport.child.is_none());
        assert!(transport.is_alive(), "a transport with no child yet has lost nothing");
    }

    /// The model rides a flag only when the node named one, and the user's own extra arguments
    /// come last so they can override what we chose.
    #[test]
    fn a_named_model_and_the_callers_own_arguments_reach_the_command_line() {
        let (sender, _events) = channel();
        let mut spec = LaunchSpec::new(
            crate::provider::ProviderChoice::new(crate::provider::Provider::Claude)
                .with_model("opus"),
        )
        .with_command("sh");
        spec.args = vec!["--add-dir".into(), "/tmp/shared".into()];

        let Ok(transport) = ClaudeCli::start(&spec, sender) else {
            return;
        };
        let args = &transport.launch.args;
        let model = args.iter().position(|arg| arg == "--model").expect("no --model");
        assert_eq!(args[model + 1], "opus");
        assert_eq!(args.last().map(String::as_str), Some("/tmp/shared"));
    }

    /// A provider with no CLI to delegate to is refused by name rather than spawning
    /// something arbitrary — the same refusal `acp.rs` gives, for the same reason.
    #[test]
    fn a_provider_with_no_command_is_refused_rather_than_guessed_at() {
        let (sender, _events) = channel();
        let spec =
            LaunchSpec::new(crate::provider::ProviderChoice::new(crate::provider::Provider::Kimi));
        assert!(matches!(ClaudeCli::start(&spec, sender), Err(AgentError::Refused(_))));
    }

    /// The one line of input that has to be right, and it is the one that was measured.
    #[test]
    fn a_prompt_is_the_shape_the_cli_accepted() {
        assert_eq!(
            user_message("hello"),
            json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
            })
        );
    }
}
