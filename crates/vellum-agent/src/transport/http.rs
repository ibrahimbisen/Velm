//! Talking to an API directly — including one running on the user's own machine.
//!
//! `docs/07-agent-canvas.md` §5c. Two wire shapes cover everything here:
//!
//! - **Anthropic's Messages API**, for a bare Claude key.
//! - **OpenAI's chat completions**, which is also how **Kimi, llama.cpp, LM Studio, Ollama
//!   and vLLM** are supported — *without a fourth code path*. That is the whole reason
//!   [`crate::provider::Provider::Custom`] exists: an OpenAI-compatible endpoint needs no
//!   code, only a base URL.
//!
//! Blocking `ureq` on a worker thread posting through a channel, the shape `vellum-link`
//! already uses. No `tokio`; see [`crate::transport`]'s module note.
//!
//! # No model id is written down here
//!
//! [`crate::provider::ProviderChoice::model`] is `Option<String>` and `None` means *the
//! provider's own current default*. Both APIs require the field, so "the provider's default"
//! has to be **asked for**: [`resolve_model`] lists the provider's models and takes the
//! newest. A pinned constant would be wrong within months and would silently override the
//! model the user's account is actually entitled to — and this file is in a public
//! repository, where a stale model id reads as a recommendation.
//!
//! # ⚠ The API key
//!
//! It is read from a file the user owns, it is put in one header, and it goes nowhere else.
//! Not into an error message, not into a `TranscriptEvent`, not into a `Debug` impl — see
//! [`Credentials`], whose `Debug` prints counts. The repository is public and a key in a
//! fixture is exactly how one gets published.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use crate::provider::{Provider, Transport as TransportKind};
use crate::transcript::{TranscriptEvent, TurnId, TurnOutcome};
use crate::transport::{AgentTransport, LaunchSpec};
use crate::{AgentError, Result};

/// The credentials file, under Velm's data directory.
pub const CREDENTIALS_FILE: &str = "credentials.json";

/// The Anthropic API version header. Their API is versioned by date and this is a wire
/// constant rather than a model id — it does not go stale on a model release.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The cap on one response.
///
/// Anthropic requires `max_tokens` and OpenAI does not, so this is only sent on the
/// Anthropic shape. It is a **length cap, not a model fact**: every current Claude model
/// accepts it, and a user who needs a longer single answer is asking for a config field
/// rather than a different constant.
const MAX_TOKENS: u32 = 8192;

/// How much answer text is accumulated before an event is emitted.
///
/// A `Text` event per token would make the transcript file mostly punctuation and JSON
/// braces — measured in bytes rather than characters because nothing here ever slices the
/// string, it only appends and flushes whole.
const FLUSH_BYTES: usize = 240;

/// The most answer text one turn will accumulate.
///
/// A provider that loops — or a local server misconfigured into repeating itself — would
/// otherwise grow one `String` until the machine gave out. The turn ends `Exhausted`, which
/// is what that state is for.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// A browser-shaped, honest user agent. Named as ourselves, exactly as `vellum-link`'s is.
const USER_AGENT: &str = concat!("Velm/", env!("CARGO_PKG_VERSION"), " (agent canvas)");

// ---------------------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------------------

/// API keys, read from `<data-dir>/credentials.json`.
///
/// ```json
/// { "claude": "sk-…", "kimi": "…" }
/// ```
///
/// Keyed by [`Provider::tag`], which is the same stable string a board file uses, so adding
/// a provider does not change the file format. The file is created mode `0600` and its
/// permissions are re-asserted on every write — a key that started life world-readable
/// stays that way otherwise.
///
/// **Never logged.** The `Debug` impl prints which providers have a key and not one byte of
/// any key; a `#[derive(Debug)]` here would put every key into the first `dbg!` anyone
/// writes, and into any panic message that formats a struct holding one.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    keys: BTreeMap<String, String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("providers", &self.keys.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// The file's path under a data directory.
    pub fn path_in(data_dir: impl AsRef<Path>) -> PathBuf {
        data_dir.as_ref().join(CREDENTIALS_FILE)
    }

    /// Reads the file. **A missing file is an empty set, not an error** — nobody has signed
    /// in yet, which is the state every install starts in.
    pub fn load(data_dir: impl AsRef<Path>) -> Result<Self> {
        let path = Self::path_in(data_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(AgentError::file(path.display().to_string(), &error)),
        };
        // A malformed file degrades to "no keys" rather than failing the launch: the remedy
        // is to sign in again, and an unreadable credentials file must not stop a board with
        // no HTTP agents on it from opening.
        Ok(Self { keys: serde_json::from_str(&text).unwrap_or_default() })
    }

    /// The key for one provider: the file first, then the environment.
    ///
    /// The environment is a genuine second source rather than a convenience — a user who
    /// already exports `ANTHROPIC_API_KEY` for their own tools should not have to copy it
    /// into a second file, and a CI run has nowhere to put a file.
    pub fn key_for(&self, provider: Provider) -> Option<String> {
        if let Some(key) = self.keys.get(provider.tag()) {
            let key = key.trim();
            if !key.is_empty() {
                return Some(key.to_owned());
            }
        }
        let variable = match provider {
            Provider::Claude => "ANTHROPIC_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Kimi => "MOONSHOT_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            // A local model needs none, and a custom endpoint's variable is the user's own
            // to name — guessing one would send a key to an endpoint they did not choose.
            Provider::Local | Provider::Custom => return None,
        };
        std::env::var(variable).ok().map(|key| key.trim().to_owned()).filter(|key| !key.is_empty())
    }

    /// Whether a provider has a key from either source.
    pub fn has_key(&self, provider: Provider) -> bool {
        self.key_for(provider).is_some()
    }

    /// The providers with a key **in the file**, for a settings panel. Never the keys.
    pub fn providers(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    /// Records a key and rewrites the file at mode `0600`.
    ///
    /// An empty key removes the entry, which is how signing out works — writing an empty
    /// string instead would leave a key-shaped hole that `key_for` has to special-case.
    pub fn store(data_dir: impl AsRef<Path>, provider: Provider, key: &str) -> Result<()> {
        let directory = data_dir.as_ref();
        let mut credentials = Self::load(directory)?;
        let key = key.trim();
        if key.is_empty() {
            credentials.keys.remove(provider.tag());
        } else {
            credentials.keys.insert(provider.tag().to_owned(), key.to_owned());
        }

        std::fs::create_dir_all(directory)
            .map_err(|error| AgentError::file(directory.display().to_string(), &error))?;
        let path = Self::path_in(directory);
        let text = serde_json::to_string_pretty(&credentials.keys)?;

        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
        file.write_all(text.as_bytes())
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;

        // `create` only sets the mode when the file is new, so a file that already existed
        // keeps whatever permissions it had — including a world-readable set from a user who
        // wrote it by hand.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Whether the file is readable by anyone but its owner. For a warning in Preferences;
    /// **not** a refusal, because refusing to read it would lock the user out of their own
    /// key over a permission bit they can fix.
    #[cfg(unix)]
    pub fn is_world_readable(data_dir: impl AsRef<Path>) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(Self::path_in(data_dir))
            .map(|metadata| metadata.permissions().mode() & 0o077 != 0)
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------------------

/// Which request body and which response shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `POST {base}/v1/messages`, `x-api-key`, `anthropic-version`.
    Anthropic,
    /// `POST {base}/chat/completions`, `Authorization: Bearer`. The base is expected to be
    /// the OpenAI-compatible root — `https://api.openai.com/v1`, `http://localhost:11434/v1`.
    OpenAi,
}

/// One turn of the conversation, as this module keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Message {
    /// `"user"` or `"assistant"`; the system context is not a message on either wire.
    role: &'static str,
    content: String,
}

/// Everything a worker needs, resolved once at start.
///
/// `key` is here and nowhere else, and this struct has **no `Debug`** for that reason.
struct Config {
    wire: Wire,
    provider: Provider,
    base: String,
    key: Option<String>,
    system: String,
    model: Option<String>,
}

impl Config {
    fn from_spec(spec: &LaunchSpec) -> Result<Self> {
        let provider = spec.provider.provider;

        // Gemini's own endpoint is `generativelanguage.googleapis.com`, which speaks neither
        // shape. Named as a gap rather than pointed at the OpenAI arm, where every request
        // would 404 and read as a broken key.
        if provider == Provider::Gemini && spec.base_url.is_none() {
            return Err(AgentError::Refused(
                "Gemini's own API is neither of the two shapes Velm speaks — run it through \
                 the `gemini` CLI (the Agent protocol transport, which also uses your \
                 subscription), or give this node an OpenAI-compatible base URL"
                    .into(),
            ));
        }

        let base = spec
            .resolved_base_url()
            .ok_or_else(|| {
                AgentError::Refused(format!(
                    "{} has no address — set this node's base URL to the endpoint it should \
                     talk to (an OpenAI-compatible one, e.g. http://localhost:11434/v1)",
                    provider.label()
                ))
            })?
            .trim_end_matches('/')
            .to_owned();

        let wire = if provider == Provider::Claude { Wire::Anthropic } else { Wire::OpenAi };

        let key = spec.api_key.clone().filter(|key| !key.trim().is_empty()).or_else(|| {
            let directory = spec.data_dir.as_ref()?;
            Credentials::load(directory).ok()?.key_for(provider)
        });
        if provider.needs_api_key() && key.is_none() {
            return Err(AgentError::Unauthorized {
                provider: provider.label().to_owned(),
                message: "there is no API key for this provider — add one in Preferences, or \
                          run it through its CLI on your subscription instead"
                    .into(),
            });
        }

        Ok(Self {
            wire,
            provider,
            base,
            key,
            system: spec.system_context.clone(),
            model: spec.provider.model.clone(),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        match self.wire {
            Wire::Anthropic => format!("{}/v1/{path}", self.base),
            Wire::OpenAi => format!("{}/{path}", self.base),
        }
    }
}

/// The request body for one turn.
///
/// Pure, so the shape of what goes on the wire is a unit test rather than a network call —
/// which is the half most likely to be wrong and the half no offline test could otherwise
/// reach.
fn request_body(wire: Wire, model: &str, system: &str, history: &[Message], stream: bool) -> Value {
    let turns: Vec<Value> = history
        .iter()
        .map(|message| json!({ "role": message.role, "content": message.content }))
        .collect();

    match wire {
        Wire::Anthropic => {
            let mut body = json!({
                "model": model,
                "max_tokens": MAX_TOKENS,
                "messages": turns,
                "stream": stream,
            });
            // The system context is a top-level field here, not a message — sending it as a
            // `{"role":"system"}` turn is a 400 on this API.
            if !system.is_empty() {
                body["system"] = json!(system);
            }
            body
        }
        Wire::OpenAi => {
            let mut messages = Vec::with_capacity(turns.len() + 1);
            if !system.is_empty() {
                messages.push(json!({ "role": "system", "content": system }));
            }
            messages.extend(turns);
            json!({ "model": model, "messages": messages, "stream": stream })
        }
    }
}

/// One thing an SSE line meant.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// A reasoning delta. Anthropic spells it `thinking_delta`; the OpenAI-compatible
    /// servers that expose reasoning spell it `reasoning_content`, which is why this arm is
    /// not Anthropic-only.
    Thought(String),
    /// The provider said why it stopped.
    Stop(TurnOutcome),
    /// The provider reported an error mid-stream.
    Failed(String),
    /// End of stream.
    Done,
    /// A keepalive, a block boundary, a usage report — nothing to show.
    Ignore,
}

/// Reads one `data:` payload.
///
/// Unknown event types are [`Piece::Ignore`], never a failure: a provider adding an event
/// type is a newer provider, not a broken session — the same posture `read_kind` takes with
/// an unknown board token.
fn parse_event(wire: Wire, data: &str) -> Piece {
    let data = data.trim();
    if data.is_empty() {
        return Piece::Ignore;
    }
    if data == "[DONE]" {
        return Piece::Done;
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return Piece::Ignore;
    };

    match wire {
        Wire::Anthropic => match value["type"].as_str().unwrap_or_default() {
            "content_block_delta" => {
                let delta = &value["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => Piece::Text(delta["text"].as_str().unwrap_or("").to_owned()),
                    "thinking_delta" => {
                        Piece::Thought(delta["thinking"].as_str().unwrap_or("").to_owned())
                    }
                    _ => Piece::Ignore,
                }
            }
            "message_delta" => match value["delta"]["stop_reason"].as_str() {
                Some(reason) => Piece::Stop(anthropic_outcome(reason)),
                None => Piece::Ignore,
            },
            "message_stop" => Piece::Done,
            "error" => Piece::Failed(
                value["error"]["message"].as_str().unwrap_or("the provider reported an error").to_owned(),
            ),
            _ => Piece::Ignore,
        },
        Wire::OpenAi => {
            if let Some(message) = value["error"]["message"].as_str() {
                return Piece::Failed(message.to_owned());
            }
            let choice = &value["choices"][0];
            if let Some(text) = choice["delta"]["content"].as_str()
                && !text.is_empty()
            {
                return Piece::Text(text.to_owned());
            }
            if let Some(text) = choice["delta"]["reasoning_content"].as_str()
                && !text.is_empty()
            {
                return Piece::Thought(text.to_owned());
            }
            match choice["finish_reason"].as_str() {
                Some(reason) => Piece::Stop(openai_outcome(reason)),
                None => Piece::Ignore,
            }
        }
    }
}

/// Reads a whole, non-streamed response.
///
/// The fallback for a server that ignores `stream` — several local ones do — and for one
/// that refuses it outright.
fn parse_complete(wire: Wire, value: &Value) -> (String, Option<String>, TurnOutcome) {
    match wire {
        Wire::Anthropic => {
            if let Some(message) = value["error"]["message"].as_str() {
                return (String::new(), None, TurnOutcome::Failed { message: message.to_owned() });
            }
            let mut text = String::new();
            let mut thinking = String::new();
            if let Some(blocks) = value["content"].as_array() {
                for block in blocks {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => text.push_str(block["text"].as_str().unwrap_or_default()),
                        "thinking" => {
                            thinking.push_str(block["thinking"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
            }
            let outcome = value["stop_reason"]
                .as_str()
                .map_or(TurnOutcome::Completed, anthropic_outcome);
            (text, (!thinking.is_empty()).then_some(thinking), outcome)
        }
        Wire::OpenAi => {
            if let Some(message) = value["error"]["message"].as_str() {
                return (String::new(), None, TurnOutcome::Failed { message: message.to_owned() });
            }
            let choice = &value["choices"][0];
            let text = choice["message"]["content"].as_str().unwrap_or_default().to_owned();
            let thinking = choice["message"]["reasoning_content"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned);
            let outcome =
                choice["finish_reason"].as_str().map_or(TurnOutcome::Completed, openai_outcome);
            (text, thinking, outcome)
        }
    }
}

/// Anthropic's stop reasons, mapped so every transport reports the same four outcomes.
fn anthropic_outcome(reason: &str) -> TurnOutcome {
    match reason {
        "max_tokens" => TurnOutcome::Exhausted {
            message: "the answer reached this request's length cap".into(),
        },
        "refusal" => TurnOutcome::Failed { message: "the model declined this request".into() },
        // `end_turn`, `stop_sequence`, `tool_use`, `pause_turn` and anything newer are an
        // answer that finished. An unknown reason must not read as a failure.
        _ => TurnOutcome::Completed,
    }
}

/// OpenAI's finish reasons, mapped the same way.
fn openai_outcome(reason: &str) -> TurnOutcome {
    match reason {
        "length" => TurnOutcome::Exhausted {
            message: "the answer reached this request's length cap".into(),
        },
        "content_filter" => {
            TurnOutcome::Failed { message: "the provider filtered this response".into() }
        }
        _ => TurnOutcome::Completed,
    }
}

// ---------------------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------------------

/// An agent that is an API call.
pub struct HttpTransport {
    config: Arc<Config>,
    history: Arc<Mutex<Vec<Message>>>,
    model: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
    events: Sender<TranscriptEvent>,
    turn: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpTransport")
            .field("wire", &self.config.wire)
            .field("provider", &self.config.provider.tag())
            .field("base", &self.config.base)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    /// Resolves the endpoint and the key. **Contacts nothing** — a session that has not been
    /// prompted must cost no request, which is `docs/07` §0's "no idle cost" one level down.
    pub fn start(spec: &LaunchSpec, events: Sender<TranscriptEvent>) -> Result<Self> {
        let config = Config::from_spec(spec)?;
        Ok(Self {
            model: Arc::new(Mutex::new(config.model.clone())),
            config: Arc::new(config),
            history: Arc::new(Mutex::new(Vec::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            events,
            turn: None,
        })
    }

    /// Whether a turn's worker is still running.
    fn busy(&self) -> bool {
        self.turn.as_ref().is_some_and(|handle| !handle.is_finished())
    }
}

impl AgentTransport for HttpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Http
    }

    fn send_prompt(&mut self, turn: TurnId, prompt: &str) -> Result<()> {
        if self.busy() {
            return Err(AgentError::Refused(
                "this agent is still answering — the prompt was not sent".into(),
            ));
        }
        drop(self.turn.take());
        self.cancel.store(false, Ordering::Relaxed);

        let _ =
            self.events.send(TranscriptEvent::TurnStarted { turn, prompt: prompt.to_owned() });
        lock(&self.history).push(Message { role: "user", content: prompt.to_owned() });

        let config = Arc::clone(&self.config);
        let history = Arc::clone(&self.history);
        let model = Arc::clone(&self.model);
        let cancel = Arc::clone(&self.cancel);
        let events = self.events.clone();
        let handle = std::thread::Builder::new()
            .name("velm-agent-http".into())
            .spawn(move || {
                let outcome = run_turn(&config, &history, &model, &cancel, &events);
                let _ = events.send(TranscriptEvent::TurnEnded { turn, outcome });
            })
            .map_err(AgentError::Io)?;
        self.turn = Some(handle);
        Ok(())
    }

    /// Asks the worker to stop.
    ///
    /// Checked between streamed chunks, so it takes effect at the next one rather than
    /// instantly — a request already in the provider's hands cannot be unsent, and this is
    /// the honest bound on what "cancel" can mean over HTTP.
    fn cancel(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Ends the session **without waiting for a turn in flight**.
    ///
    /// Joining would block quitting the app for as long as the provider takes to answer,
    /// which is minutes on a hard question. The worker sees the cancel flag, or its `send`
    /// fails once the receiver is gone, and exits either way.
    fn shutdown(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Relaxed);
        drop(self.turn.take());
        Ok(())
    }
}

/// One turn, on the worker thread. Answers the outcome; the caller emits `TurnEnded`.
fn run_turn(
    config: &Config,
    history: &Mutex<Vec<Message>>,
    model: &Mutex<Option<String>>,
    cancel: &AtomicBool,
    events: &Sender<TranscriptEvent>,
) -> TurnOutcome {
    let named = {
        let cached = lock(model).clone();
        match cached {
            Some(name) => name,
            None => match resolve_model(config) {
                Ok(name) => {
                    *lock(model) = Some(name.clone());
                    name
                }
                Err(error) => return TurnOutcome::Failed { message: error.to_string() },
            },
        }
    };

    let turns = lock(history).clone();
    let answer = match exchange(config, &named, &turns, cancel, events) {
        Ok(answer) => answer,
        Err(error) => return TurnOutcome::Failed { message: error.to_string() },
    };

    // The answer joins the history **whatever the outcome**, including a cancelled or
    // exhausted one: the provider has already said it, so a follow-up question that pretended
    // otherwise would be answering against a conversation that never happened.
    if !answer.text.is_empty() {
        lock(history).push(Message { role: "assistant", content: answer.text });
    }
    answer.outcome
}

/// What one exchange produced.
struct Answer {
    text: String,
    outcome: TurnOutcome,
}

/// Makes the request and turns the response into events.
fn exchange(
    config: &Config,
    model: &str,
    history: &[Message],
    cancel: &AtomicBool,
    events: &Sender<TranscriptEvent>,
) -> Result<Answer> {
    let body = request_body(config.wire, model, &config.system, history, true);
    let response = post(config, &config.endpoint(match config.wire {
        Wire::Anthropic => "messages",
        Wire::OpenAi => "chat/completions",
    }), &body)?;

    let status = response.status().as_u16();
    let streaming = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("event-stream"));

    if !(200..300).contains(&status) {
        let mut response = response;
        let text = response.body_mut().read_to_string().unwrap_or_default();
        return Err(status_error(config, status, &text));
    }

    if !streaming {
        // A server that ignored `stream` answered the whole thing at once. Not an error and
        // not worth a second request — several local servers do exactly this.
        let mut response = response;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| transport_error(&error.to_string()))?;
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            transport_error("the provider's answer was neither a stream nor JSON")
        })?;
        let (answer, thinking, outcome) = parse_complete(config.wire, &value);
        if let Some(thinking) = thinking {
            let _ = events.send(TranscriptEvent::Thought { text: thinking });
        }
        if !answer.is_empty() {
            let _ = events.send(TranscriptEvent::Text { text: answer.clone() });
        }
        return Ok(Answer { text: answer, outcome });
    }

    read_stream(config.wire, response, cancel, events)
}

/// Drains an SSE body, emitting coalesced events as it goes.
fn read_stream(
    wire: Wire,
    response: ureq::http::Response<ureq::Body>,
    cancel: &AtomicBool,
    events: &Sender<TranscriptEvent>,
) -> Result<Answer> {
    let mut reader = BufReader::new(response.into_body().into_reader());
    let mut line = String::new();
    let mut answer = String::new();
    let mut pending = String::new();
    let mut outcome = TurnOutcome::Completed;
    let mut exhausted = false;

    loop {
        if cancel.load(Ordering::Relaxed) {
            flush(events, &mut pending);
            return Ok(Answer { text: answer, outcome: TurnOutcome::Cancelled });
        }
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| transport_error(&error.to_string()))?;
        if read == 0 {
            break;
        }
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            // `event:`, `id:`, a comment (`:` keepalive), or the blank line between events.
            continue;
        };

        match parse_event(wire, data) {
            Piece::Text(text) => {
                answer.push_str(&text);
                pending.push_str(&text);
                if pending.len() >= FLUSH_BYTES || pending.ends_with('\n') {
                    flush(events, &mut pending);
                }
                if answer.len() > MAX_ANSWER_BYTES {
                    exhausted = true;
                    break;
                }
            }
            Piece::Thought(text) => {
                flush(events, &mut pending);
                if events.send(TranscriptEvent::Thought { text }).is_err() {
                    break;
                }
            }
            Piece::Stop(reported) => outcome = reported,
            Piece::Failed(message) => {
                flush(events, &mut pending);
                return Ok(Answer { text: answer, outcome: TurnOutcome::Failed { message } });
            }
            Piece::Done => break,
            Piece::Ignore => {}
        }
    }

    flush(events, &mut pending);
    if exhausted {
        outcome = TurnOutcome::Exhausted {
            message: "the answer grew past what one turn will hold".into(),
        };
    }
    Ok(Answer { text: answer, outcome })
}

fn flush(events: &Sender<TranscriptEvent>, pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    let _ = events.send(TranscriptEvent::Text { text: std::mem::take(pending) });
}

/// Asks the provider what its current model is called.
///
/// This is what `None` means (see the module note). Newest first: the OpenAI shape carries a
/// `created` timestamp and Anthropic's list is already newest-first, so the largest `created`
/// wins where there is one and the first entry wins where there is not.
fn resolve_model(config: &Config) -> Result<String> {
    let url = config.endpoint("models");
    let mut response = get(config, &url)?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(status_error(config, status, &text));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| transport_error("the provider's model list was not JSON"))?;
    let listed = value["data"].as_array().cloned().unwrap_or_default();

    newest_id(&listed).ok_or_else(|| {
        AgentError::Refused(format!(
            "{} did not say which models it has, so there is nothing to run — name a model on \
             this node",
            config.provider.label()
        ))
    })
}

/// Which of a listed set of models is the current one.
///
/// Two shapes, one rule. The OpenAI shape carries a `created` timestamp, so the largest wins;
/// Anthropic's list carries none and is already newest-first, so the **first** wins.
///
/// ⚠ The order of those two cases is the behaviour. `max_by_key` answers the *last* of an
/// equal set, so applying it to a list with no timestamps at all silently picks the oldest
/// model the provider offers — which looks like a working default and is the wrong model for
/// the life of the session.
fn newest_id(listed: &[Value]) -> Option<String> {
    let named: Vec<&Value> = listed.iter().filter(|entry| entry["id"].is_string()).collect();
    let dated = named.iter().any(|entry| entry["created"].as_i64().is_some());
    let chosen = if dated {
        named.iter().copied().max_by_key(|entry| entry["created"].as_i64().unwrap_or(i64::MIN))
    } else {
        named.first().copied()
    }?;
    chosen["id"].as_str().map(str::to_owned)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        // A non-2xx is a *response*, not a transport failure: the body carries the
        // provider's own explanation, which is the only useful thing to put in the toast,
        // and `http_status_as_error` would throw it away.
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        // Deliberately no global timeout: a streamed answer to a hard question runs for
        // minutes, and a clock that cut it off would look exactly like the model giving up.
        .build()
        .into()
}

fn post(config: &Config, url: &str, body: &Value) -> Result<ureq::http::Response<ureq::Body>> {
    let mut request = agent().post(url).header("content-type", "application/json");
    request = authorise(request, config);
    request
        .send(serde_json::to_string(body)?)
        .map_err(|error| transport_error(&error.to_string()))
}

fn get(config: &Config, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    let mut request = agent().get(url);
    request = authorise(request, config);
    request.call().map_err(|error| transport_error(&error.to_string()))
}

/// Attaches the key, in the one header each wire wants it in.
fn authorise<T>(
    request: ureq::RequestBuilder<T>,
    config: &Config,
) -> ureq::RequestBuilder<T> {
    let Some(key) = config.key.as_deref() else {
        return request;
    };
    match config.wire {
        Wire::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION),
        Wire::OpenAi => request.header("authorization", format!("Bearer {key}")),
    }
}

/// Turns a non-2xx into an error that names the provider and quotes *its* explanation.
///
/// The body is the provider's own message and never contains the key — the key went out in
/// a header. Truncated by **characters**, because a provider's error page can be a megabyte
/// of HTML and `&text[..300]` panics on any multi-byte character straddling the boundary
/// (feedback 30, twice, with `panic = "abort"` in the release profile).
fn status_error(config: &Config, status: u16, body: &str) -> AgentError {
    let detail: String = body.trim().chars().take(300).collect();
    let detail = if detail.is_empty() { format!("HTTP {status}") } else { detail };
    if matches!(status, 401 | 403) {
        AgentError::Unauthorized { provider: config.provider.label().to_owned(), message: detail }
    } else {
        AgentError::Transport { transport: "http", message: format!("HTTP {status}: {detail}") }
    }
}

fn transport_error(message: &str) -> AgentError {
    AgentError::Transport { transport: "http", message: message.to_owned() }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderChoice;

    fn history() -> Vec<Message> {
        vec![
            Message { role: "user", content: "hello".into() },
            Message { role: "assistant", content: "hi".into() },
            Message { role: "user", content: "again".into() },
        ]
    }

    /// The two shapes differ in exactly the places that 400 if they are wrong: Anthropic
    /// requires `max_tokens` and puts the system context at the top level, while OpenAI
    /// takes it as the first message and rejects `max_tokens`-less requests happily.
    #[test]
    fn each_wire_puts_the_system_context_where_that_api_wants_it() {
        let anthropic = request_body(Wire::Anthropic, "a-model", "be terse", &history(), true);
        assert_eq!(anthropic["system"], "be terse");
        assert_eq!(anthropic["max_tokens"], MAX_TOKENS);
        assert_eq!(anthropic["messages"].as_array().unwrap().len(), 3, "a system turn leaked in");
        assert_eq!(anthropic["messages"][0]["role"], "user");
        assert_eq!(anthropic["stream"], true);

        let openai = request_body(Wire::OpenAi, "a-model", "be terse", &history(), false);
        assert!(openai.get("system").is_none(), "a top-level system field 400s on this API");
        assert_eq!(openai["messages"].as_array().unwrap().len(), 4);
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][1]["content"], "hello");
        assert_eq!(openai["stream"], false);

        // An empty context adds nothing at all rather than an empty string, which some
        // OpenAI-compatible servers reject outright.
        let bare = request_body(Wire::OpenAi, "m", "", &history(), true);
        assert_eq!(bare["messages"].as_array().unwrap().len(), 3);
        assert!(request_body(Wire::Anthropic, "m", "", &history(), true).get("system").is_none());
    }

    /// The framing, against real event lines. This is the part most likely to be wrong and
    /// the part no offline test could reach any other way.
    #[test]
    fn anthropic_stream_events_map_to_the_right_transcript_events() {
        let text = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#;
        assert_eq!(parse_event(Wire::Anthropic, text), Piece::Text("Hel".into()));

        let thinking = r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        assert_eq!(parse_event(Wire::Anthropic, thinking), Piece::Thought("hmm".into()));

        let ended = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}"#;
        assert_eq!(parse_event(Wire::Anthropic, ended), Piece::Stop(TurnOutcome::Completed));

        let capped = r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#;
        assert!(matches!(
            parse_event(Wire::Anthropic, capped),
            Piece::Stop(TurnOutcome::Exhausted { .. })
        ));

        let failed = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert_eq!(parse_event(Wire::Anthropic, failed), Piece::Failed("Overloaded".into()));

        assert_eq!(parse_event(Wire::Anthropic, r#"{"type":"message_stop"}"#), Piece::Done);
        // A block boundary and a ping carry nothing, and an event type from a later API
        // version must be ignored rather than ending the turn.
        assert_eq!(parse_event(Wire::Anthropic, r#"{"type":"ping"}"#), Piece::Ignore);
        assert_eq!(
            parse_event(Wire::Anthropic, r#"{"type":"a_future_event","x":1}"#),
            Piece::Ignore
        );
        // Malformed JSON on the wire is an ignored line, not a dead session.
        assert_eq!(parse_event(Wire::Anthropic, "{not json"), Piece::Ignore);
    }

    #[test]
    fn openai_stream_events_map_to_the_right_transcript_events() {
        let text = r#"{"choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#;
        assert_eq!(parse_event(Wire::OpenAi, text), Piece::Text("Hel".into()));

        // The reasoning field the OpenAI-compatible reasoning servers use — Kimi and
        // DeepSeek both — which is why this arm is not Anthropic-only.
        let thinking = r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#;
        assert_eq!(parse_event(Wire::OpenAi, thinking), Piece::Thought("hmm".into()));

        let ended = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_event(Wire::OpenAi, ended), Piece::Stop(TurnOutcome::Completed));

        let capped = r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#;
        assert!(matches!(
            parse_event(Wire::OpenAi, capped),
            Piece::Stop(TurnOutcome::Exhausted { .. })
        ));

        assert_eq!(parse_event(Wire::OpenAi, "[DONE]"), Piece::Done);
        assert_eq!(
            parse_event(Wire::OpenAi, r#"{"error":{"message":"no such model"}}"#),
            Piece::Failed("no such model".into())
        );
        assert_eq!(parse_event(Wire::OpenAi, r#"{"choices":[{"delta":{}}]}"#), Piece::Ignore);
    }

    /// A server that ignores `stream` answers the whole thing at once — several local ones
    /// do — and that must be an answer rather than a failure.
    #[test]
    fn a_whole_response_is_read_when_the_server_did_not_stream() {
        let anthropic: Value = serde_json::from_str(
            r#"{"content":[{"type":"thinking","thinking":"weighing"},
                 {"type":"text","text":"the answer"}],"stop_reason":"end_turn"}"#,
        )
        .unwrap();
        let (text, thinking, outcome) = parse_complete(Wire::Anthropic, &anthropic);
        assert_eq!(text, "the answer");
        assert_eq!(thinking.as_deref(), Some("weighing"));
        assert_eq!(outcome, TurnOutcome::Completed);

        let openai: Value = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"local answer"},
                 "finish_reason":"length"}]}"#,
        )
        .unwrap();
        let (text, _, outcome) = parse_complete(Wire::OpenAi, &openai);
        assert_eq!(text, "local answer");
        assert!(matches!(outcome, TurnOutcome::Exhausted { .. }));

        let refused: Value =
            serde_json::from_str(r#"{"error":{"message":"model not found"}}"#).unwrap();
        let (text, _, outcome) = parse_complete(Wire::OpenAi, &refused);
        assert!(text.is_empty());
        assert!(matches!(outcome, TurnOutcome::Failed { .. }));
    }

    /// Both providers' "I stopped early" reasons must reach the **same** outcome, or the
    /// node would say "finished" on one provider and "ran out" on another for one situation.
    #[test]
    fn the_two_apis_stop_reasons_agree_on_one_set_of_outcomes() {
        assert_eq!(anthropic_outcome("end_turn"), TurnOutcome::Completed);
        assert_eq!(anthropic_outcome("tool_use"), TurnOutcome::Completed);
        assert_eq!(openai_outcome("stop"), TurnOutcome::Completed);
        assert_eq!(openai_outcome("tool_calls"), TurnOutcome::Completed);

        for capped in [anthropic_outcome("max_tokens"), openai_outcome("length")] {
            assert!(matches!(capped, TurnOutcome::Exhausted { .. }), "{capped:?}");
        }
        // A reason neither of us has heard of is a finished answer, not a failure.
        assert_eq!(anthropic_outcome("something_new"), TurnOutcome::Completed);
        assert_eq!(openai_outcome("something_new"), TurnOutcome::Completed);
    }

    /// The claim in the module note, as an assertion: nothing in this file writes a model
    /// id, so there is nothing to go stale.
    #[test]
    fn no_model_id_is_written_down_in_this_module() {
        let source = include_str!("http.rs");
        // The needles are joined at runtime from halves, or this assertion would find its
        // own haystack — a whole model id written here is a match for itself, which is how
        // the first version of this test failed on the one file it was checking.
        for shape in [["claude", "-"], ["gpt", "-4"], ["moonshot", "-v"], ["llama", "-3"]] {
            let needle = shape.concat();
            assert!(!source.contains(&needle), "a model id reached the source: {needle}");
        }
    }

    /// Gemini is neither shape at its own endpoint. Pointing it at the OpenAI arm would 404
    /// every request and read as a bad key, so it is named as a gap — and a user who *has*
    /// an OpenAI-compatible proxy for it is still allowed through.
    #[test]
    fn gemini_over_http_is_refused_by_name_unless_a_compatible_endpoint_is_given() {
        let spec = LaunchSpec::new(
            ProviderChoice::new(Provider::Gemini).with_transport(TransportKind::Http),
        );
        let Err(error) = Config::from_spec(&spec) else {
            panic!("a config that should not resolve did");
        };
        assert!(error.to_string().contains("gemini"), "{error}");

        let proxied = LaunchSpec {
            base_url: Some("http://localhost:8080/v1".into()),
            api_key: Some("not-a-real-key".into()),
            ..spec
        };
        let config = Config::from_spec(&proxied).expect("a compatible endpoint is allowed");
        assert_eq!(config.wire, Wire::OpenAi);
        assert_eq!(config.endpoint("chat/completions"), "http://localhost:8080/v1/chat/completions");
    }

    /// A local model needs no key and must not be refused for want of one — that is the
    /// entire point of running one.
    #[test]
    fn a_local_model_starts_with_no_key_and_a_metered_one_refuses_without_it() {
        let local = LaunchSpec {
            base_url: Some("http://localhost:11434/v1/".into()),
            ..LaunchSpec::new(ProviderChoice::new(Provider::Local))
        };
        let config = Config::from_spec(&local).expect("a local model needs no key");
        assert!(config.key.is_none());
        // The trailing slash is trimmed once, here, rather than at every call site.
        assert_eq!(config.endpoint("models"), "http://localhost:11434/v1/models");

        let kimi = LaunchSpec {
            // An empty data directory, so the file lookup finds nothing.
            data_dir: Some(PathBuf::from("/nonexistent-velm-test")),
            ..LaunchSpec::new(ProviderChoice::new(Provider::Kimi))
        };
        // Only meaningful when the environment does not carry one; a machine that exports
        // `MOONSHOT_API_KEY` is entitled to be signed in already.
        if std::env::var("MOONSHOT_API_KEY").is_err() {
            let Err(error) = Config::from_spec(&kimi) else {
                panic!("a config that should not resolve did");
            };
            assert!(matches!(error, AgentError::Unauthorized { .. }), "{error}");
        }

        let claude = LaunchSpec {
            api_key: Some("supplied-by-the-caller".into()),
            ..LaunchSpec::new(ProviderChoice::new(Provider::Claude))
        };
        let config = Config::from_spec(&claude).unwrap();
        assert_eq!(config.wire, Wire::Anthropic);
        assert_eq!(config.endpoint("messages"), "https://api.anthropic.com/v1/messages");
    }

    /// The key is the one thing in this crate that must never be printed. A `Debug` that
    /// leaked it would put it in every panic message that formats a struct holding one — and
    /// this repository is public.
    #[test]
    fn a_credentials_debug_never_prints_a_key() {
        let scratch = tempfile::tempdir().unwrap();
        Credentials::store(scratch.path(), Provider::Claude, "sk-secret-value").unwrap();

        let credentials = Credentials::load(scratch.path()).unwrap();
        assert_eq!(credentials.key_for(Provider::Claude).as_deref(), Some("sk-secret-value"));

        let printed = format!("{credentials:?}");
        assert!(!printed.contains("sk-secret-value"), "the key was printed: {printed}");
        assert!(printed.contains("claude"), "the provider should still be nameable: {printed}");
        assert_eq!(credentials.providers(), vec!["claude".to_owned()]);
    }

    /// The file holds a secret, so it is created private and **stays** private when it is
    /// rewritten — `create` only applies the mode to a file that did not exist.
    #[test]
    #[cfg(unix)]
    fn the_credentials_file_is_written_private_even_when_it_already_existed() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir().unwrap();
        let path = Credentials::path_in(scratch.path());

        // A file the user wrote by hand, world-readable.
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Credentials::is_world_readable(scratch.path()));

        Credentials::store(scratch.path(), Provider::Kimi, "a-key").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the credentials file was left readable: {mode:o}");
        assert!(!Credentials::is_world_readable(scratch.path()));
    }

    #[test]
    fn a_missing_credentials_file_is_an_empty_set_and_an_empty_key_signs_out() {
        let scratch = tempfile::tempdir().unwrap();
        let empty = Credentials::load(scratch.path()).unwrap();
        assert!(empty.providers().is_empty());
        assert!(!empty.has_key(Provider::Local), "a local model never has a key");

        Credentials::store(scratch.path(), Provider::Claude, "k").unwrap();
        assert!(Credentials::load(scratch.path()).unwrap().has_key(Provider::Claude));
        Credentials::store(scratch.path(), Provider::Claude, "  ").unwrap();
        assert!(Credentials::load(scratch.path()).unwrap().providers().is_empty());

        // A corrupt file degrades to "no keys" rather than stopping a board from opening.
        std::fs::write(Credentials::path_in(scratch.path()), b"not json").unwrap();
        assert!(Credentials::load(scratch.path()).unwrap().providers().is_empty());
    }

    /// Newest first, and a list with no timestamps still answers something rather than
    /// failing — an OpenAI-compatible local server often omits `created` entirely.
    #[test]
    fn the_model_list_picks_the_newest_and_tolerates_a_list_without_timestamps() {
        // The real function, not a copy of it: a second implementation in the test is a
        // test that agrees with itself and with nothing that ships.
        fn pick(json: &str) -> Option<String> {
            let value: Value = serde_json::from_str(json).unwrap();
            let listed = value["data"].as_array().cloned().unwrap_or_default();
            newest_id(&listed)
        }

        assert_eq!(
            pick(r#"{"data":[{"id":"older","created":100},{"id":"newer","created":200}]}"#),
            Some("newer".into())
        );
        assert_eq!(pick(r#"{"data":[{"id":"only-one"},{"id":"second"}]}"#), Some("only-one".into()));
        assert_eq!(pick(r#"{"data":[]}"#), None);
    }
}
