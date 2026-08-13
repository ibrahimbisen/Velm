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
use std::io::{BufRead, BufReader, Read, Write};
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

/// The most a turn will produce, **text and thinking together**.
///
/// A provider that loops — or a local server misconfigured into repeating itself — would
/// otherwise grow one `String` until the machine gave out. The turn ends `Exhausted`, which
/// is what that state is for.
///
/// ⚠ It used to count `Piece::Text` alone, which left a provider emitting nothing but
/// `thinking_delta` unbounded — the cap was on whichever half was in mind when it was written,
/// and both halves cross the same channel into the same transcript.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// The longest single line the body reader will hold.
///
/// One SSE frame is a sentence. A body with no newline in it at all — a server answering a
/// stream request with a megabyte of HTML, a proxy's error page — would otherwise be one
/// `String` grown by `read_line` **before** [`MAX_ANSWER_BYTES`] is ever consulted, which is
/// the wrong order: that constant bounds what the *answer* accumulates and cannot bound what
/// reading one line costs. A line past this is cut and the remainder arrives as the next
/// line, where it fails to parse as `data:` and is skipped.
const MAX_LINE_BYTES: u64 = 1024 * 1024;

/// The most raw body bytes one exchange will read, across every line.
///
/// The aggregate half of [`MAX_LINE_BYTES`]: a stream of well-formed short frames that never
/// ends is bounded by [`MAX_ANSWER_BYTES`] only in what it *keeps*, and a provider emitting
/// nothing but ignorable events would be read forever.
const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;

/// How long the body may go without producing a **single byte** before the turn is failed.
///
/// ⚠ **A stall timeout, not a deadline, and the difference is the whole point.** A total
/// budget — `ureq`'s own `timeout_recv_body`, or `voice.rs`'s `timeout_global` — cuts off a
/// long answer that is arriving perfectly well, which looks exactly like the model giving up
/// mid-sentence; that is why [`agent`] deliberately sets no global timeout. What has to be
/// caught is the *other* shape: a socket that sends its headers and then goes quiet forever.
/// Measured from the last byte, so an answer that keeps producing is never interrupted however
/// long it runs, and one that produces nothing ends the turn instead of wedging the node —
/// `busy()` is `!handle.is_finished()`, so a worker blocked on a dead socket refuses every
/// later prompt for the life of the session.
const STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the consumer wakes while waiting for a line.
///
/// Also what makes **cancel work while stalled**: the old blocking `read_line` checked the
/// flag only between lines, so a stalled stream ignored the user's stop as well.
const STALL_TICK: Duration = Duration::from_millis(200);

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

    /// Records a key for one provider, in memory. Persist with [`Credentials::save`].
    ///
    /// An empty key **removes** rather than storing nothing: a sign-in dialog confirmed with
    /// a cleared field means "forget this", and storing `""` would leave a provider that
    /// reports `has_key` and then fails every request with an empty `authorization` header —
    /// a state that looks configured and is not.
    pub fn set_key(&mut self, provider: Provider, key: &str) {
        if key.trim().is_empty() {
            self.remove_key(provider);
        } else {
            self.keys.insert(provider.tag().to_owned(), key.trim().to_owned());
        }
    }

    pub fn remove_key(&mut self, provider: Provider) {
        self.keys.remove(provider.tag());
    }

    /// Writes the file, mode `0600`, atomically.
    ///
    /// Three things this does deliberately, each because the alternative has a real failure:
    ///
    /// - **Permissions are set on the temporary file *before* the key is written into it**,
    ///   not on the final file afterwards. Creating a world-readable file, writing a secret
    ///   into it and then narrowing it leaves a window in which any process on the machine
    ///   can read it, and that window is exactly when the interesting bytes are there.
    /// - **Temp then rename**, so a crash mid-write cannot leave a truncated file that reads
    ///   as "no keys" and silently signs the user out.
    /// - The temp file is in the **same directory**, because a rename across filesystems is
    ///   a copy and is not atomic.
    pub fn save(&self, data_dir: impl AsRef<Path>) -> Result<()> {
        let path = Self::path_in(&data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AgentError::file(parent.display().to_string(), &error))?;
        }
        let temp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(&self.keys)?;

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            use std::io::Write;
            let mut file = options
                .open(&temp)
                .map_err(|error| AgentError::file(temp.display().to_string(), &error))?;
            file.write_all(body.as_bytes())
                .map_err(|error| AgentError::file(temp.display().to_string(), &error))?;
            file.sync_all()
                .map_err(|error| AgentError::file(temp.display().to_string(), &error))?;
        }
        std::fs::rename(&temp, &path)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;

        // Re-asserted after the rename as well: a file that already existed keeps its own
        // permissions through a rename, so a credentials file that started life
        // world-readable would stay that way for ever without this.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
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
    /// Content **and** a stop reason in the same frame.
    ///
    /// Not a tidiness variant — it is a shape the OpenAI wire genuinely sends, and reading it
    /// as either half alone loses the other. Returning `Text` early meant the stop was
    /// dropped, so a stream that *had* said it finished reached EOF with no terminator and was
    /// reported as cut short: the fix for "a truncated stream is reported as complete" turned
    /// into "a complete stream is reported as truncated", which is the same defect facing the
    /// other way and every bit as much of a lie.
    TextThenStop(String, TurnOutcome),
    /// The same frame for a **reasoning** delta, and it is not a hypothetical corner.
    ///
    /// `TextThenStop` was added for `content` and applied to `content` alone — one branch over,
    /// `reasoning_content` went on returning a bare `Thought` and dropping the `finish_reason`
    /// beside it. So a reasoning turn cut short by `length` or stopped by `content_filter` was
    /// reported as complete, which is the defect that variant exists to prevent, surviving in
    /// its sibling. The servers that send reasoning this way — Kimi and DeepSeek, named in
    /// `parse_event`'s own comment — are exactly the ones that attach the stop reason to the
    /// last chunk rather than sending a frame of its own.
    ThoughtThenStop(String, TurnOutcome),
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
                error_message(&value)
                    .unwrap_or_else(|| "the provider reported an error".to_owned()),
            ),
            _ => Piece::Ignore,
        },
        Wire::OpenAi => {
            // ⚠ **Both error shapes, and the second one is the one that bites.** Ollama sends
            // `{"error":"model 'x' not found"}` — a bare string where the wire documents an
            // object — and reading only `error.message` made that frame a `Piece::Ignore`.
            // Ignored, the stream then reaches EOF with no terminator, which used to be
            // reported as a *completed* turn: the node went idle, in green, having said
            // nothing, over a failure the provider had named in the first frame.
            if let Some(message) = error_message(&value) {
                return Piece::Failed(message);
            }
            let choice = &value["choices"][0];
            let stop = choice["finish_reason"].as_str().map(openai_outcome);
            if let Some(text) = choice["delta"]["content"].as_str()
                && !text.is_empty()
            {
                return match stop {
                    Some(outcome) => Piece::TextThenStop(text.to_owned(), outcome),
                    None => Piece::Text(text.to_owned()),
                };
            }
            // The same two shapes as `content` above, and they have to be read the same way:
            // the stop reason rides on the last chunk, which for a reasoning turn is a
            // reasoning chunk. Returning a bare `Thought` here dropped it.
            if let Some(text) = choice["delta"]["reasoning_content"].as_str()
                && !text.is_empty()
            {
                return match stop {
                    Some(outcome) => Piece::ThoughtThenStop(text.to_owned(), outcome),
                    None => Piece::Thought(text.to_owned()),
                };
            }
            match stop {
                Some(outcome) => Piece::Stop(outcome),
                None => Piece::Ignore,
            }
        }
    }
}

/// The provider's own error message, in **either** shape it arrives in.
///
/// `{"error": {"message": "…"}}` is what both wires document and what the hosted providers
/// send. `{"error": "…"}` — a bare string — is what Ollama sends, and it is not a rare corner:
/// *"model 'x' not found"* is the first thing a user meets when a local model name is wrong.
/// Reading only the object form turned that into no message at all, which then degraded into
/// an empty answer and a completed turn.
fn error_message(value: &Value) -> Option<String> {
    match &value["error"] {
        Value::String(message) if !message.trim().is_empty() => Some(message.clone()),
        Value::Object(_) => Some(
            value["error"]["message"]
                .as_str()
                .unwrap_or("the provider reported an error it did not describe")
                .to_owned(),
        ),
        _ => None,
    }
}

/// What a body *was*, for a failure that has to name a shape it did not recognise.
///
/// **The field names, never the values.** A 200 that is not an answer can be a captive
/// portal's login page, a proxy's error document, or the user's own prompt handed back; none
/// of that belongs in a message on the canvas, and the useful half — *"it sent `model`,
/// `done`, `response`"* — is what tells whoever reads it which server they are actually
/// talking to.
fn describe_shape(value: &Value) -> String {
    match value {
        Value::Object(map) if map.is_empty() => "an empty JSON object".to_owned(),
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let listed = keys.len();
            keys.truncate(8);
            let named = keys.join(", ");
            if listed > 8 {
                format!("a JSON object whose fields begin {named} (and {} more)", listed - 8)
            } else {
                format!("a JSON object with the field(s) {named}")
            }
        }
        Value::Array(items) => format!("a JSON array of {} item(s)", items.len()),
        Value::Null => "a JSON null".to_owned(),
        _ => "a JSON scalar".to_owned(),
    }
}

/// The failure a 200 with nothing in it deserves.
///
/// ⚠ **Silence reported as success is the worst answer this transport can give.** An agent
/// that stopped for a reason nobody was told is one the user will believe: the node goes
/// `Idle`, in the colour that means it worked, with no error and no detail. A named failure
/// costs the user one glance and is always recoverable; a false success is not.
fn empty_answer(value: &Value) -> TurnOutcome {
    TurnOutcome::Failed {
        message: format!(
            "the provider answered 200 with no content and no stop reason, so nothing was \
             said and nothing explained why. It sent {}.",
            describe_shape(value)
        ),
    }
}

/// Reads a whole, non-streamed response.
///
/// The fallback for a server that ignores `stream` — several local ones do — and for one
/// that refuses it outright.
fn parse_complete(wire: Wire, value: &Value) -> (String, Option<String>, TurnOutcome) {
    match wire {
        Wire::Anthropic => {
            if let Some(message) = error_message(value) {
                return (String::new(), None, TurnOutcome::Failed { message });
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
            // A body with nothing in it and no reason for having nothing in it is **not** a
            // completed turn. `{"choices":[]}`, `{"error":"…"}` in a shape we did not read, an
            // OpenAI-ish server answering an Anthropic-ish request — all three used to arrive
            // here as an empty string and `Completed`.
            let Some(reason) = value["stop_reason"].as_str() else {
                if text.is_empty() && thinking.is_empty() {
                    return (String::new(), None, empty_answer(value));
                }
                return (text, (!thinking.is_empty()).then_some(thinking), TurnOutcome::Completed);
            };
            (text, (!thinking.is_empty()).then_some(thinking), anthropic_outcome(reason))
        }
        Wire::OpenAi => {
            if let Some(message) = error_message(value) {
                return (String::new(), None, TurnOutcome::Failed { message });
            }
            let choice = &value["choices"][0];
            let text = choice["message"]["content"].as_str().unwrap_or_default().to_owned();
            let thinking = choice["message"]["reasoning_content"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned);
            let Some(reason) = choice["finish_reason"].as_str() else {
                if text.is_empty() && thinking.is_none() {
                    return (String::new(), None, empty_answer(value));
                }
                return (text, thinking, TurnOutcome::Completed);
            };
            (text, thinking, openai_outcome(reason))
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

        let config = Arc::clone(&self.config);
        let history = Arc::clone(&self.history);
        let model = Arc::clone(&self.model);
        let cancel = Arc::clone(&self.cancel);
        let events = self.events.clone();
        let asked = prompt.to_owned();
        // ⚠ **`TurnStarted` and the history entry are both the worker's, and neither may
        // happen before the spawn.** `AgentTransport::send_prompt`'s contract is that an
        // `Err` means no `TurnStarted` was emitted, and `.spawn(…)?` sat *below* the emit: a
        // failed spawn returned `Err`, `Session::dispatch` correctly cleared its own state,
        // and the `TurnStarted` was already in the channel. The next `poll` absorbed it, set
        // `Status::Running`, and nothing ever emitted the `TurnEnded` that closes it — so
        // every later prompt was refused with *"this agent is still answering"* for the life
        // of the session. Trap 11's shape: a `?` on the unwind path of a paired begin/end.
        //
        // The other two transports moved this into their closures and this one did not. In
        // here the pairing is structural — the same closure emits both events and nothing
        // else in this transport emits either — rather than careful.
        //
        // The user's message goes in with it, because a prompt that was never sent must not
        // be in the conversation the *next* one is built from. It is read only by `run_turn`,
        // three lines below, so moving it changes nothing anyone else can observe.
        let handle = std::thread::Builder::new()
            .name("velm-agent-http".into())
            .spawn(move || {
                let _ = events.send(TranscriptEvent::TurnStarted { turn, prompt: asked.clone() });
                lock(&history).push(Message { role: "user", content: asked });
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
        //
        // Read through the same pump as the stream, so the stall timeout covers it too: a
        // socket that sends headers and goes quiet wedges a whole-body read exactly as it
        // wedges a streamed one, and `read_to_string` has no clock of its own.
        let lines = spawn_line_reader(response);
        let Some(text) = read_body(&lines, cancel, STALL_TIMEOUT)? else {
            return Ok(Answer { text: String::new(), outcome: TurnOutcome::Cancelled });
        };
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
    let lines = spawn_line_reader(response);
    consume_stream(wire, &lines, cancel, events, STALL_TIMEOUT)
}

/// Hands the body's lines back one at a time, from a thread that owns the socket.
///
/// # Why the read is on its own thread
///
/// A blocking `read_line` cannot be given a stall timeout: the only clocks `ureq` offers are
/// *total* budgets, and a total budget on a streamed answer cuts off a model that is still
/// talking (see [`STALL_TIMEOUT`]). Moving the read one thread away means the consumer waits
/// on a channel instead of a socket, and a channel can be waited on with a deadline.
///
/// ⚠ **A stalled reader is leaked, deliberately, and the leak is bounded.** The thread stays
/// blocked on a socket nobody can interrupt from here; it ends when the peer or the OS finally
/// closes the connection, and at worst when the process does. One thread and one socket per
/// stalled turn is the price of a node that stays usable, against a node that is wedged for
/// the life of the session — and the wedge is not hypothetical, `busy()` is
/// `!handle.is_finished()`, so the *next* prompt and every prompt after it is refused.
///
/// The reader is capped twice: [`MAX_LINE_BYTES`] per line, so no single unterminated line can
/// be allocated whole, and [`MAX_STREAM_BYTES`] across the body.
fn spawn_line_reader(
    response: ureq::http::Response<ureq::Body>,
) -> std::sync::mpsc::Receiver<std::io::Result<String>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    // Cloned into the worker so the original survives a failed `spawn` — `Builder::spawn`
    // drops the closure it could not run, and with it the only sender, which would reach the
    // consumer as a clean end of body. Dropped below, or the channel never disconnects and
    // end of body is indistinguishable from a stall.
    let worker = sender.clone();
    let spawned = std::thread::Builder::new()
        .name("velm-agent-http-body".into())
        .spawn(move || {
            let source: Box<dyn std::io::Read + Send> =
                Box::new(response.into_body().into_reader());
            pump_lines(BufReader::new(source.take(MAX_STREAM_BYTES)), &worker);
        });
    if let Err(error) = spawned {
        // No thread, so nothing will ever send: hand the failure over the channel rather than
        // returning an empty one, or the consumer reads it as a clean end of body — which is
        // the "silence reported as success" this whole path exists to refuse.
        let _ = sender.send(Err(error));
    }
    drop(sender);
    receiver
}

/// Splits a body into lines and posts each one, bounded **per line**.
///
/// ⚠ **The cap is applied before the allocation, not after it.** `read_line` grows its target
/// until it meets a newline and only then can anything be checked — so a body with no newline
/// in it is allocated whole first and refused second, which is not a bound. `read_until` on a
/// `Take` stops at [`MAX_LINE_BYTES`] instead; the remainder of an over-long line arrives as
/// the next line, where it fails to parse as `data:` and is skipped.
///
/// Takes any [`BufRead`] rather than the response, so this is an offline test over a byte
/// slice — the socket half of [`spawn_line_reader`] has no test seam at all.
fn pump_lines(mut reader: impl BufRead, sender: &Sender<std::io::Result<String>>) {
    loop {
        let mut raw: Vec<u8> = Vec::new();
        match (&mut reader).take(MAX_LINE_BYTES).read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {
                if sender.send(Ok(String::from_utf8_lossy(&raw).into_owned())).is_err() {
                    // The consumer gave up — a cancel, or a stall it has already reported.
                    // There is nothing left to hand anywhere.
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error));
                break;
            }
        }
    }
}

/// What the line pump produced, or why it produced nothing.
enum Next {
    Line(String),
    /// The body ended.
    Ended,
    /// Nothing at all arrived inside the stall window.
    Stalled,
    /// The user pressed stop.
    Cancelled,
}

/// One line, waiting no longer than the stall window for it.
///
/// `idle` is time since the **last byte**, carried by the caller and reset on every line, so a
/// long answer that keeps arriving is never interrupted. The wait is broken into
/// [`STALL_TICK`]s rather than taken in one `recv_timeout`, which is also what lets a cancel
/// land while the stream is quiet — the old blocking read only tested the flag between lines.
fn next_line(
    lines: &std::sync::mpsc::Receiver<std::io::Result<String>>,
    cancel: &AtomicBool,
    stall: Duration,
    idle: &mut Duration,
) -> Result<Next> {
    let tick = STALL_TICK.min(stall);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Next::Cancelled);
        }
        match lines.recv_timeout(tick) {
            Ok(Ok(line)) => {
                *idle = Duration::ZERO;
                return Ok(Next::Line(line));
            }
            Ok(Err(error)) => return Err(transport_error(&error.to_string())),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                *idle = idle.saturating_add(tick);
                if *idle >= stall {
                    return Ok(Next::Stalled);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(Next::Ended),
        }
    }
}

/// Reads a whole body through the pump, so it is stall-bounded like the streamed one.
///
/// `None` means the user cancelled. It is **not** the empty string: a partial body parsed as
/// though it were the whole answer is a truncated answer reported as a finished one, which is
/// the same defect this module has in the streaming path and deserves the same refusal.
fn read_body(
    lines: &std::sync::mpsc::Receiver<std::io::Result<String>>,
    cancel: &AtomicBool,
    stall: Duration,
) -> Result<Option<String>> {
    let mut body = String::new();
    let mut idle = Duration::ZERO;
    loop {
        match next_line(lines, cancel, stall, &mut idle)? {
            Next::Line(line) => body.push_str(&line),
            Next::Ended => return Ok(Some(body)),
            Next::Stalled => return Err(stalled_error(stall)),
            Next::Cancelled => return Ok(None),
        }
    }
}

/// Consumes the SSE frames, emitting coalesced events as it goes.
///
/// Separated from [`spawn_line_reader`] so every rule below is an ordinary offline test: the
/// tests push lines down the channel by hand, close it without a terminator, and hold one back
/// to make the stall fire.
///
/// # ⚠ A stream that stops is not a stream that finished
///
/// `outcome` starts as `Completed` because that is what a well-formed stream ends up as, and
/// for a long time nothing but `Piece::Stop` ever reassigned it. So a body reaching **clean
/// EOF** with no `[DONE]`, no `message_stop` and no `finish_reason` — a dropped connection, a
/// proxy that closed early, a local server killed mid-answer — was indistinguishable from one
/// that finished: the node went `Idle` with no error and no detail, mid-sentence. `terminator`
/// is what tells the two apart, and an agent that stopped silently is one the user will
/// believe, which is why it is a failure rather than a warning.
fn consume_stream(
    wire: Wire,
    lines: &std::sync::mpsc::Receiver<std::io::Result<String>>,
    cancel: &AtomicBool,
    events: &Sender<TranscriptEvent>,
    stall: Duration,
) -> Result<Answer> {
    let mut answer = String::new();
    let mut pending = String::new();
    let mut outcome = TurnOutcome::Completed;
    let mut exhausted = false;
    let mut terminator = false;
    // Text **and** thinking, against one budget. Counting only the answer left a provider
    // that emits nothing but `thinking_delta` unbounded — the cap was on the half that
    // happened to be in mind when it was written.
    let mut produced = 0usize;
    let mut idle = Duration::ZERO;

    loop {
        let line = match next_line(lines, cancel, stall, &mut idle)? {
            Next::Line(line) => line,
            Next::Ended => break,
            Next::Cancelled => {
                flush(events, &mut pending);
                return Ok(Answer { text: answer, outcome: TurnOutcome::Cancelled });
            }
            Next::Stalled => {
                flush(events, &mut pending);
                return Ok(Answer {
                    text: answer,
                    outcome: TurnOutcome::Failed { message: stalled_message(stall) },
                });
            }
        };

        let Some(data) = line.trim_end().strip_prefix("data:") else {
            // `event:`, `id:`, a comment (`:` keepalive), or the blank line between events.
            continue;
        };

        match parse_event(wire, data) {
            Piece::Text(text) => {
                produced += text.len();
                answer.push_str(&text);
                pending.push_str(&text);
                if pending.len() >= FLUSH_BYTES || pending.ends_with('\n') {
                    flush(events, &mut pending);
                }
                if produced > MAX_ANSWER_BYTES {
                    exhausted = true;
                    break;
                }
            }
            Piece::Thought(text) => {
                produced += text.len();
                flush(events, &mut pending);
                if events.send(TranscriptEvent::Thought { text }).is_err() {
                    break;
                }
                if produced > MAX_ANSWER_BYTES {
                    exhausted = true;
                    break;
                }
            }
            Piece::Stop(reported) => {
                terminator = true;
                outcome = reported;
            }
            // The reasoning is delivered before the stop is recorded, exactly as the text arm
            // below does it. `produced` counts it for the same reason `Thought` does: a
            // provider that emits nothing but reasoning must still reach `MAX_ANSWER_BYTES`.
            Piece::ThoughtThenStop(text, reported) => {
                produced += text.len();
                flush(events, &mut pending);
                let sent = events.send(TranscriptEvent::Thought { text }).is_ok();
                terminator = true;
                outcome = reported;
                if !sent {
                    break;
                }
            }
            // The text is delivered *before* the stop is recorded, so the last words of an
            // answer reach the node even though the same frame ended the turn.
            Piece::TextThenStop(text, reported) => {
                answer.push_str(&text);
                produced += text.len();
                pending.push_str(&text);
                flush(events, &mut pending);
                terminator = true;
                outcome = reported;
            }
            Piece::Failed(message) => {
                flush(events, &mut pending);
                return Ok(Answer { text: answer, outcome: TurnOutcome::Failed { message } });
            }
            Piece::Done => {
                terminator = true;
                break;
            }
            Piece::Ignore => {}
        }
    }

    flush(events, &mut pending);
    if exhausted {
        // Our own stop, and a legitimate end: the turn says how far it got.
        return Ok(Answer {
            text: answer,
            outcome: TurnOutcome::Exhausted {
                message: "the answer grew past what one turn will hold".into(),
            },
        });
    }
    if !terminator {
        return Ok(Answer {
            outcome: TurnOutcome::Failed {
                message: format!(
                    "the provider's stream ended after {} character(s) without saying the \
                     answer was finished — no stop reason and no end-of-stream marker. \
                     Whatever is on the node is very likely cut short.",
                    answer.chars().count()
                ),
            },
            text: answer,
        });
    }
    Ok(Answer { text: answer, outcome })
}

fn stalled_message(stall: Duration) -> String {
    format!(
        "the provider stopped sending: nothing arrived for {}s while the connection stayed \
         open. The answer, if there was one, is unfinished.",
        stall.as_secs().max(1)
    )
}

fn stalled_error(stall: Duration) -> AgentError {
    transport_error(&stalled_message(stall))
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

        // ⚠ **With the stop reason attached, which is what the last chunk of a real stream
        // carries.** The line above omits `finish_reason` — the field production supplies —
        // so it passed on a build that read the reasoning and threw the stop away, and a
        // reasoning turn cut off at the token cap was reported as a finished answer. The two
        // servers named above are the ones that put it here rather than in a frame of its own.
        let capped_thought =
            r#"{"choices":[{"delta":{"reasoning_content":"hm"},"finish_reason":"length"}]}"#;
        assert!(
            matches!(
                parse_event(Wire::OpenAi, capped_thought),
                Piece::ThoughtThenStop(ref text, TurnOutcome::Exhausted { .. })
                    if text.as_str() == "hm"
            ),
            "a capped reasoning chunk dropped its stop reason: {:?}",
            parse_event(Wire::OpenAi, capped_thought)
        );
        let filtered =
            r#"{"choices":[{"delta":{"reasoning_content":"hm"},"finish_reason":"content_filter"}]}"#;
        assert!(
            !matches!(parse_event(Wire::OpenAi, filtered), Piece::Thought(_)),
            "a filtered reasoning chunk was reported as an ordinary thought"
        );

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

    /// **A 200 whose shape we do not recognise is a failure, not an empty answer.**
    ///
    /// Three bodies, all of which used to reach the node as a *completed* turn that said
    /// nothing: Ollama's string-shaped error, an empty `choices` list, and a JSON document
    /// from something that is not this API at all. The node went `Idle`, in the colour that
    /// means it worked, with no error and no detail — and an agent that stopped silently is
    /// one the user will believe.
    ///
    /// The failure must **name the shape** so whoever reads it can tell which server they
    /// are talking to, and must not quote the body, which can be a login page.
    #[test]
    fn a_two_hundred_that_is_not_an_answer_is_reported_as_a_failure() {
        // Ollama, verbatim: an error as a bare string where the wire documents an object.
        let ollama: Value =
            serde_json::from_str(r#"{"error":"model 'llama9' not found, try pulling it"}"#)
                .unwrap();
        let (text, _, outcome) = parse_complete(Wire::OpenAi, &ollama);
        assert!(text.is_empty());
        let TurnOutcome::Failed { message } = outcome else {
            panic!("a string-shaped error was not a failure");
        };
        assert!(message.contains("llama9"), "the provider's own words were dropped: {message}");

        // The same string shape on the Anthropic wire, and mid-stream.
        let (_, _, outcome) = parse_complete(Wire::Anthropic, &ollama);
        assert!(matches!(outcome, TurnOutcome::Failed { .. }));
        assert_eq!(
            parse_event(Wire::OpenAi, r#"{"error":"no such model"}"#),
            Piece::Failed("no such model".into()),
            "a string-shaped error frame was ignored, so the stream ended with no terminator"
        );

        // No content and no finish reason. Nothing said, nothing explaining why.
        let empty: Value = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        let (text, _, outcome) = parse_complete(Wire::OpenAi, &empty);
        assert!(text.is_empty());
        let TurnOutcome::Failed { message } = outcome else {
            panic!("an empty choices list was reported as a completed turn");
        };
        assert!(message.contains("choices"), "the failure did not name the shape: {message}");

        let foreign: Value =
            serde_json::from_str(r#"{"model":"x","done":true,"response":""}"#).unwrap();
        let (_, _, outcome) = parse_complete(Wire::Anthropic, &foreign);
        let TurnOutcome::Failed { message } = outcome else {
            panic!("a body from another API was reported as a completed turn");
        };
        assert!(message.contains("done") && message.contains("model"), "{message}");
        assert!(!message.contains("\"x\""), "the body's values reached the message: {message}");

        // ⚠ The other half: a legitimately empty answer that *says* why is still not a
        // failure. A tool-call-only turn carries no text and a real stop reason.
        let tools: Value = serde_json::from_str(
            r#"{"choices":[{"message":{"content":null},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        let (_, _, outcome) = parse_complete(Wire::OpenAi, &tools);
        assert_eq!(outcome, TurnOutcome::Completed, "a stop reason was ignored");
    }

    /// Feeds the consumer lines as though the reader thread had produced them.
    ///
    /// Everything below the socket is exercised: the channel is the seam, so a stall is a
    /// sender held open and an end of body is a sender dropped.
    fn consume(
        wire: Wire,
        lines: &[&str],
        stall: Duration,
    ) -> (Result<Answer>, Vec<TranscriptEvent>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        for line in lines {
            sender.send(Ok((*line).to_owned())).unwrap();
        }
        drop(sender);
        let (events, drained) = std::sync::mpsc::channel();
        let cancel = AtomicBool::new(false);
        let answer = consume_stream(wire, &receiver, &cancel, &events, stall);
        drop(events);
        (answer, drained.try_iter().collect())
    }

    /// ⚠ **A stream that stops is not a stream that finished.**
    ///
    /// `outcome` starts as `Completed` and only `Piece::Stop` ever moved it, so a body
    /// reaching clean EOF with no `[DONE]`, no `message_stop` and no `finish_reason` — a
    /// dropped connection, a proxy that closed early, a local server killed mid-answer — was
    /// indistinguishable from one that finished. The node went `Idle` with no error and no
    /// detail, mid-sentence. **Reporting a failed turn as a completed one is the worst thing
    /// this transport can do**, because there is nothing on screen to disbelieve.
    ///
    /// A/B: with `terminator` forced to `true` the first case reports `Completed` and every
    /// other assertion here still passes — which is why the *absence* is what is asserted.
    #[test]
    fn a_stream_that_ends_without_saying_so_is_a_failure_and_not_a_completed_turn() {
        let cut_short = [
            r#"data: {"choices":[{"delta":{"content":"the first half of a "}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"sentence that never"}}]}"#,
        ];
        let (answer, _) = consume(Wire::OpenAi, &cut_short, Duration::from_millis(50));
        let answer = answer.expect("a truncated body is an answer with a bad outcome, not an Err");
        assert_eq!(answer.text, "the first half of a sentence that never");
        let TurnOutcome::Failed { message } = answer.outcome else {
            panic!("a truncated stream was reported as a completed turn");
        };
        assert!(message.contains("cut short"), "{message}");

        // The three ways a stream *does* say it finished, none of which may fail.
        let (done, _) = consume(
            Wire::OpenAi,
            &[r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#, "data: [DONE]"],
            Duration::from_millis(50),
        );
        assert_eq!(done.unwrap().outcome, TurnOutcome::Completed);

        let (stopped, _) = consume(
            Wire::OpenAi,
            &[r#"data: {"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#],
            Duration::from_millis(50),
        );
        // Both halves of that frame, because it carries both. Asserting only the outcome
        // would pass on a build that reported `Completed` and threw the last words away —
        // which is what the first version of `Piece::TextThenStop` would have done if it had
        // recorded the stop and dropped the text.
        let stopped = stopped.unwrap();
        assert_eq!(stopped.outcome, TurnOutcome::Completed);
        assert_eq!(stopped.text, "hi", "a frame carrying content and a stop lost its content");

        let (anthropic, _) = consume(
            Wire::Anthropic,
            &[
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
            Duration::from_millis(50),
        );
        assert_eq!(anthropic.unwrap().outcome, TurnOutcome::Completed);

        // A provider that named its own failure is already a failure and must keep *its*
        // words rather than being overwritten by the missing-terminator message.
        let (failed, _) = consume(
            Wire::OpenAi,
            &[r#"data: {"error":"no such model"}"#],
            Duration::from_millis(50),
        );
        let TurnOutcome::Failed { message } = failed.unwrap().outcome else {
            panic!("a named provider error was not a failure");
        };
        assert_eq!(message, "no such model");
    }

    /// **A stall, not a deadline.** A socket that sends its headers and then goes quiet used
    /// to block the worker thread forever — and `busy()` is `!handle.is_finished()`, so the
    /// node refused every later prompt for the life of the session with *"this agent is still
    /// answering"*.
    ///
    /// The distinction is asserted in both directions: a quiet connection ends the turn, and
    /// a slow one that keeps producing does **not** — which is the reason a total budget
    /// (`ureq`'s own `timeout_recv_body`) is the wrong tool and is deliberately not set.
    #[test]
    fn a_body_that_goes_quiet_ends_the_turn_and_a_slow_one_does_not() {
        // ⚠ The margin between the feeder's gaps and the stall window is **10×**, not 3×, and
        // that is deliberate. `cargo test` runs ~65 binaries in parallel on this machine and
        // `glass_budget.rs` is on record overrunning a wall-clock budget by 2× under exactly
        // that load — a `sleep(45ms)` that oversleeps past the window would report the slow
        // half as a stall, which is a red run that means nothing. The stall-*fires* half needs
        // no margin: it can only run long, never wrong.
        let stall = Duration::from_millis(450);

        // The sender is held open with nothing on it: the connection is alive and silent.
        let (_held, receiver) = std::sync::mpsc::channel::<std::io::Result<String>>();
        let (events, _drained) = std::sync::mpsc::channel();
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let answer = consume_stream(Wire::OpenAi, &receiver, &cancel, &events, stall)
            .expect("a stall is an outcome, not a transport Err");
        assert!(started.elapsed() < Duration::from_secs(5), "the stall never fired");
        let TurnOutcome::Failed { message } = answer.outcome else {
            panic!("a silent connection was not reported");
        };
        assert!(message.contains("stopped sending"), "{message}");

        // A body arriving in pieces, each one inside the window but the whole run well past
        // it. A deadline would cut this off; a stall must not.
        let (sender, receiver) = std::sync::mpsc::channel();
        let feeder = std::thread::spawn(move || {
            for index in 0..6 {
                std::thread::sleep(Duration::from_millis(45));
                let line = format!(
                    r#"data: {{"choices":[{{"delta":{{"content":"{index}"}}}}]}}"#
                );
                if sender.send(Ok(line)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Ok("data: [DONE]".to_owned()));
        });
        let (events, _drained) = std::sync::mpsc::channel();
        let answer = consume_stream(Wire::OpenAi, &receiver, &cancel, &events, stall).unwrap();
        let _ = feeder.join();
        assert_eq!(answer.text, "012345", "a slow but progressing answer was cut off");
        assert_eq!(answer.outcome, TurnOutcome::Completed);
    }

    /// The two allocation bounds on a body, both of which used to be checked after the
    /// allocation they were meant to prevent.
    ///
    /// `MAX_ANSWER_BYTES` counted only `Piece::Text`, so a provider emitting nothing but
    /// `thinking_delta` was unbounded — the cap was on whichever half was in mind when it
    /// was written. And `read_line` grows its target until it finds a newline, so a body with
    /// no newline in it at all was allocated whole before anything could refuse it.
    #[test]
    fn a_body_is_bounded_per_line_and_thinking_counts_against_the_answer_cap() {
        // A megabyte of "thinking" per frame, four frames: no answer text at all, and the
        // turn must still stop.
        let chunk = "t".repeat(1_100_000);
        let frame = format!(
            r#"data: {{"type":"content_block_delta","delta":{{"type":"thinking_delta","thinking":"{chunk}"}}}}"#
        );
        let lines = [frame.as_str(); 5];
        let (answer, _) = consume(Wire::Anthropic, &lines, Duration::from_millis(50));
        let answer = answer.unwrap();
        assert!(answer.text.is_empty(), "thinking is not answer text");
        assert!(
            matches!(answer.outcome, TurnOutcome::Exhausted { .. }),
            "a stream of pure thinking was unbounded: {:?}",
            answer.outcome
        );

        // One line with no newline anywhere in it. The pump must cut it rather than grow it.
        let (sender, receiver) = std::sync::mpsc::channel();
        let runaway = vec![b'x'; (MAX_LINE_BYTES as usize) + 4_096];
        pump_lines(&runaway[..], &sender);
        drop(sender);
        let pieces: Vec<String> = receiver.iter().map(|line| line.unwrap()).collect();
        assert_eq!(pieces.len(), 2, "an unterminated line was not cut: {} piece(s)", pieces.len());
        assert_eq!(pieces[0].len(), MAX_LINE_BYTES as usize);
        assert_eq!(pieces[1].len(), 4_096);

        // Ordinary lines are still whole, newline and all — the framing must survive.
        let (sender, receiver) = std::sync::mpsc::channel();
        pump_lines(&b"data: one\ndata: two\n"[..], &sender);
        drop(sender);
        let pieces: Vec<String> = receiver.iter().map(|line| line.unwrap()).collect();
        assert_eq!(pieces, vec!["data: one\n".to_owned(), "data: two\n".to_owned()]);
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
