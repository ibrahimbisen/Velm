//! Velm's own MCP server: the same surface the CLI shim offers, spoken over stdio.
//!
//! `docs/07-agent-canvas.md` §6 ends with the sentence this module exists to make true:
//! *"Agents that speak MCP get the identical surface through `velm-mcp`."* An agent client
//! that can be handed an MCP server — Claude Code, Codex, Zed, anything else that grew one —
//! gets Velm's verbs without `velm-agent-cli` having to be on its `PATH` and without the
//! agent having to know it is being asked to shell out.
//!
//! ```text
//!   agent process
//!     │  JSON-RPC 2.0, one message per line, over this process's stdin/stdout
//!     ▼
//!   velm-mcp ──┬── research_*  ──► crate::research   (the network, directly)
//!              └── velm_*      ──► crate::ipc        (the loopback server, in Velm)
//! ```
//!
//! # The two halves behave differently on purpose
//!
//! `research_search` and `research_fetch` are served **here**, in this process, because they
//! need nothing from the application: a URL and a socket. So they work when Velm is not
//! running at all, which is the case for an agent started from a terminal.
//!
//! Everything beginning `velm_` is served by **Velm**, over [`crate::ipc`]. This module holds
//! no opinion about what a note is or which agents are connected — it turns a tool call into
//! an [`ipc::Request`], sends it, and renders the [`ipc::Response`]. That is deliberate and it
//! is the whole reason the verb set is [`ipc::Request`]'s own enum rather than a list retyped
//! here: a surface that is *"exactly the verbs the shim exposes"* has to be the same enum, or
//! it is exactly the verbs the shim exposed on the day somebody wrote both lists down.
//!
//! # This module is pure, and the binary is a loop
//!
//! [`Server::handle_line`] takes a line of JSON and answers with a line of JSON or with
//! `None`. Every protocol test below is therefore a string in and a string out, with no
//! process, no pipe and no clock — and `src/bin/velm_mcp.rs` is thirty lines of read-a-line,
//! write-a-line around it.
//!
//! **Nothing in this module or that binary may write to stdout.** stdout *is* the protocol
//! channel; one stray `println!` puts a non-JSON line into the stream and the client's session
//! is over. Diagnostics go to stderr, which the client shows the user and never parses.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use serde_json::{Value, json};

use crate::ipc::{self, Answer, RuntimeFile};
use crate::research::{Research, ResearchConfig, ResearchError};

// ---------------------------------------------------------------------------------------
// The protocol's own strings
// ---------------------------------------------------------------------------------------

/// Method names and the version string, in one place.
///
/// **⚠ Every constant in here belongs to the Model Context Protocol, not to Velm.** They are
/// gathered in one module so that a protocol revision is one edit rather than a grep, and
/// because none of them can be verified from inside this repository — there is no MCP client
/// in the workspace to check them against. Treat a client that will not initialise as a
/// version question first and a bug second.
///
/// The implementation around them is deliberately tolerant of what it does not recognise:
/// [`Server::handle_line`] echoes the client's own `protocolVersion` back rather than
/// insisting on [`protocol::VERSION`], ignores any notification it has never heard of, and
/// answers an unknown *method* with a proper JSON-RPC error instead of dying. A newer client
/// is the expected case, not a broken session.
pub mod protocol {
    /// The revision this server was written against.
    ///
    /// Used only when a client does not name one of its own. See the module's doc comment for
    /// why this is a guess with a plan rather than a fact.
    pub const VERSION: &str = "2025-06-18";

    pub const INITIALIZE: &str = "initialize";
    pub const PING: &str = "ping";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";

    /// Every notification's method name starts with this.
    ///
    /// ⚠ **Naming only. It is not, and must not become, a test for "is this a
    /// notification".** JSON-RPC 2.0 defines a notification as a request with no `id`, and
    /// the method name has no say in it — so a prefix test used as an *independent*
    /// condition drops `{"id": 7, "method": "notifications/message"}`, which is a real
    /// request, and leaves the client blocked on id 7 for as long as it is willing to wait.
    /// That is exactly what [`super::Server::handle_line`] used to do. The absence of the id
    /// decides; this constant describes a namespace.
    pub const NOTIFICATION_PREFIX: &str = "notifications/";
}

/// JSON-RPC 2.0's own error numbers. Not Velm's, and not negotiable.
mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

/// The environment variable naming Velm's loopback server. See [`Endpoint`].
pub const IPC_ENV: &str = "VELM_IPC";

/// The environment variable carrying the calling node's item id.
pub const AGENT_ID_ENV: &str = "VELM_AGENT_ID";

/// The environment variable carrying the per-launch token, for the `host:port` form of
/// [`IPC_ENV`]. Unnecessary — and ignored — when `VELM_IPC` names the runtime file, which
/// already contains it.
pub const IPC_TOKEN_ENV: &str = "VELM_IPC_TOKEN";

// ---------------------------------------------------------------------------------------
// Reaching Velm
// ---------------------------------------------------------------------------------------

/// Where Velm is, and who we are claiming to be.
///
/// # The seam, stated because it has two implementors
///
/// `docs/07-agent-canvas.md` §6 says agents are launched with `VELM_IPC` and `VELM_AGENT_ID`
/// in their environment and does not say what `VELM_IPC` *contains*. It is defined here, and
/// `velm-agent-cli` must resolve it identically or the two halves of one surface disagree
/// about how to find the same server:
///
/// - **`VELM_IPC` is the path to `<data-dir>/runtime/ipc.json`** — the file [`ipc::IpcServer`]
///   writes at mode `0600`. One variable carries both the port and the token, because the two
///   are useless apart and a token in an environment variable is visible to every process that
///   can read `/proc` or run `ps -E`; a file the OS protects is the better place for it, and
///   it is where the server already puts it.
/// - **`VELM_IPC` may instead be `host:port` or a bare port**, in which case the token is read
///   from `VELM_IPC_TOKEN`. Supported because a test harness, and a user wiring this up by
///   hand, both want to say *"port 51234"* without a file — and because a resolver that
///   accepted only one shape would fail with *"no such file"* on the shape somebody guessed.
///
/// **An unset `VELM_IPC` is not an error at startup.** It means this process was not launched
/// by a Velm agent node, which is an entirely ordinary way to run: the `research_*` tools
/// still work, and only the `velm_*` tools answer with the named refusal in [`Endpoint::why`].
/// Refusing to start would take the working half of the server away with the missing half.
#[derive(Debug, Clone)]
pub struct Endpoint {
    reach: Result<Reach, String>,
}

#[derive(Clone)]
struct Reach {
    address: SocketAddr,
    token: String,
    agent: String,
}

/// ⚠ **Hand-written, because this token is the one that authorises `spawn` and `configure`.**
///
/// [`Endpoint`]'s own derive is fine and stays — a derived `Debug` prints its fields with
/// *their* impls, so redacting here redacts there. What must not happen is a derive on this
/// struct: an `Endpoint` is held for the life of the MCP server and is the natural thing to
/// print when a `velm_*` tool answers a refusal nobody expected, which is precisely the moment
/// the token would be read out into a log the user then pastes somewhere.
///
/// The address and the agent id are printed. Neither is a secret — the port is in a file on
/// disk and the agent id is in the environment — and both are what a *"which Velm is this
/// talking to"* question actually needs.
impl std::fmt::Debug for Reach {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reach")
            .field("address", &self.address)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Endpoint {
    /// Resolves [`IPC_ENV`] and [`AGENT_ID_ENV`]. Never fails — an unresolvable environment
    /// becomes a sentence the `velm_*` tools report.
    pub fn from_env() -> Self {
        Self { reach: resolve_from_env() }
    }

    /// Resolves the same two variables out of a **supplied** environment.
    ///
    /// ⚠ This is what lets a transport running *inside Velm* use the board tools. `from_env`
    /// reads `std::env`, which for an in-process transport is Velm's own environment and does
    /// not carry `VELM_IPC` — those live on [`crate::LaunchSpec::env`], because they were
    /// written to be handed to a **child process**. Without this constructor the whole board
    /// surface is reachable only by agents Velm spawns, which is exactly why feature 19 could
    /// not reach an HTTP agent: there is no child to put an environment on.
    ///
    /// Same resolution, same refusal wording, one implementation — so an HTTP agent and a
    /// spawned `claude` cannot come to disagree about what "Velm is not reachable" means.
    pub fn from_pairs(env: &[(String, String)]) -> Self {
        Self {
            reach: resolve_with(|name| {
                env.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            }),
        }
    }

    /// An endpoint that is deliberately not there, carrying the reason.
    ///
    /// Public because the protocol tests need a server whose Velm half is known-absent
    /// whatever the machine running `cargo test` happens to have in its environment. A test
    /// that read the real environment would pass on a laptop and fail inside Velm.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self { reach: Err(reason.into()) }
    }

    /// Whether the `velm_*` tools can do anything.
    pub fn is_available(&self) -> bool {
        self.reach.is_ok()
    }

    /// Why not, when not.
    pub fn why(&self) -> Option<&str> {
        self.reach.as_ref().err().map(String::as_str)
    }

    /// The calling node's id, when there is one.
    pub fn agent(&self) -> Option<&str> {
        self.reach.as_ref().ok().map(|reach| reach.agent.as_str())
    }

    /// One request to Velm, and its answer.
    ///
    /// The `Err` side is **the sentence a model reads**, so it is written to be acted on: a
    /// refusal from Velm comes back as Velm's own wording, and a failure to reach Velm at all
    /// says so rather than reporting the verb as refused. Those are different situations and a
    /// model that cannot tell them apart will either retry forever or give up too early.
    pub fn call(&self, request: ipc::Request) -> Result<Answer, String> {
        let reach = self.reach.as_ref().map_err(Clone::clone)?;
        let envelope = ipc::Envelope {
            token: reach.token.clone(),
            agent: reach.agent.clone(),
            request,
        };
        let response = ipc::call(reach.address, &envelope)
            .map_err(|error| format!("Velm could not be reached: {error}"))?;
        if response.ok {
            Ok(response.answer.unwrap_or(Answer::Done))
        } else {
            Err(response.message().to_owned())
        }
    }
}

fn resolve_from_env() -> Result<Reach, String> {
    resolve_with(non_empty)
}

/// The resolution itself, over any lookup.
///
/// Split out so [`Endpoint::from_pairs`] and [`Endpoint::from_env`] are the *same* rules — the
/// address forms, the token requirement, the runtime-file fallback and every refusal sentence.
/// Two copies of this would be two ideas of what a reachable Velm is.
fn resolve_with(look: impl Fn(&str) -> Option<String>) -> Result<Reach, String> {
    let agent = look(AGENT_ID_ENV).ok_or_else(|| {
        format!(
            "{AGENT_ID_ENV} is not set, so this process cannot say which agent node it is \
             acting for. The velm_* tools are only available to a process Velm started from \
             an agent node on a board. The research_* tools work regardless."
        )
    })?;
    let ipc = look(IPC_ENV).ok_or_else(|| {
        format!(
            "{IPC_ENV} is not set, so this process was not launched by Velm and there is no \
             board to act on. The research_* tools work regardless."
        )
    })?;

    if let Some(address) = parse_address(&ipc) {
        let token = look(IPC_TOKEN_ENV).ok_or_else(|| {
            format!(
                "{IPC_ENV} names an address ({ipc}) but {IPC_TOKEN_ENV} is not set, so there \
                 is no credential to send. Point {IPC_ENV} at Velm's runtime/ipc.json instead, \
                 which carries both."
            )
        })?;
        return Ok(Reach { address, token, agent });
    }

    let file = RuntimeFile::read(Path::new(&ipc)).map_err(|error| {
        format!("{IPC_ENV} points at {ipc}, which could not be read as Velm's runtime file: {error}")
    })?;
    Ok(Reach { address: file.address(), token: file.token, agent })
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

/// `host:port`, or a bare port meaning loopback. `None` for anything else, which is then
/// treated as a path.
fn parse_address(text: &str) -> Option<SocketAddr> {
    if let Ok(port) = text.parse::<u16>() {
        return (port != 0).then(|| SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    }
    text.parse::<SocketAddr>().ok()
}

// ---------------------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------------------

/// The MCP tools this server offers.
///
/// # Why the names have underscores where `docs/07` writes dots
///
/// A tool name reaches a model through its provider's tool-definition API, and those constrain
/// the name — Anthropic's is `^[a-zA-Z0-9_-]{1,64}$`, which a dot fails. A tool the model
/// cannot be told about is not a tool. `research.search` is therefore `research_search`; the
/// prefix still does the grouping the dot was there for, and [`ipc::Request::verb`] keeps the
/// dotted spelling on the wire, where nothing constrains it.
pub const TOOL_NAMES: [&str; 12] = [
    "research_search",
    "research_fetch",
    "velm_send_message",
    "velm_list_notes",
    "velm_read_note",
    "velm_read_note_chain",
    "velm_write_note",
    "velm_post_image",
    "velm_post_options",
    "velm_ingest_context",
    "velm_spawn_agent",
    "velm_configure_agent",
];

/// The `tools/list` payload.
///
/// # These descriptions are the only documentation the calling model will ever see
///
/// It cannot read this file, cannot see the schema's doc comments and gets no second attempt
/// at understanding a tool before it calls one. So each description says, in order: what the
/// tool *returns*, what each argument means, and **what it will refuse** — the last of which
/// is the part that stops a model discovering a boundary by walking into it and then arguing
/// with the refusal. That is why `velm_spawn_agent` names the two roles that may call it and
/// why `research_fetch` says outright that a blocked site comes back as a refusal rather than
/// as an empty page: a model told that will look elsewhere, and a model surprised by it will
/// retry.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "research_search",
            "description": "\
Search the web and get back a numbered list of results, each with a title, a URL and a short \
snippet. Use it to find pages worth reading, then pass a URL to research_fetch to read one.\n\
\n\
Arguments: `query` is the search text, exactly as you would type it into a search box — \
plain words work better than operators, because the engine behind this is chosen by the \
user's configuration and may not support them. `limit` is how many results to return \
(1-25, default 10).\n\
\n\
What it refuses: if the search endpoint answers with an error or an anti-automation page, \
this returns that refusal and its HTTP status code rather than an empty list — it does not \
retry in disguise. If the user has switched search off, it says so and names the setting. \
Either way, treat the refusal as final for this endpoint and either use a URL you already \
have or tell the user what you could not reach.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for." },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 25,
                        "description": "How many results to return. Defaults to 10."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "research_fetch",
            "description": "\
Fetch one web page and get its readable text back, with the page title, the URL actually \
read after any redirects, and a note if the page was too long to return whole. Headings keep \
their markdown `#`, list items become `- `, and links become `[text](target)` with relative \
targets already resolved, so you can pass a link straight back to this tool.\n\
\n\
Arguments: `url` must be an http:// or https:// address. Nothing else is accepted — a file \
path or a mailto: address is refused rather than read.\n\
\n\
What it does not do: it does not run JavaScript, so a page whose content is assembled in the \
browser comes back nearly empty; try a URL that serves the content directly. Navigation \
menus, footers, scripts and styles are stripped, so a very short result usually means the \
page had little prose rather than that the fetch failed.\n\
\n\
What it refuses, and this matters: Velm identifies itself honestly, keeps a cookie session, \
paces itself per site and obeys robots.txt. When a site answers 403, 404 or 429, or when its \
robots.txt disallows the path, you get that refusal with its status code or the exact rule — \
not an empty page and not a second attempt in disguise. That is a real answer: go and find \
another source, and say which site refused if it matters to the user.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http(s) URL to read."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_send_message",
            "description": "\
Send a message to another agent on the same Velm board. Returns confirmation that it was \
delivered; the other agent receives it as an incoming message on its own node and may reply \
by sending one back to you.\n\
\n\
Arguments: `to` is the other agent's node id, or the label written on its node — a label that \
matches more than one node is refused rather than guessed at. `text` is the message.\n\
\n\
What it refuses: messages only travel along a connector the user has drawn between the two \
nodes on the board, so an agent you are not connected to cannot be messaged and the refusal \
says so. Ask the user to draw a line between the nodes rather than trying another name. A \
message also carries a hop count, so a chain of agents forwarding to each other is cut off \
rather than looping.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "The target agent's node id, or the label on its node."
                    },
                    "text": { "type": "string", "description": "What to say." }
                },
                "required": ["to", "text"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_list_notes",
            "description": "\
List every note on this board that you are allowed to read: the shared notes every agent can \
see, plus your own private ones. Returns each note's path, its scope and its title.\n\
\n\
Notes are real markdown files on disk that the user can open in any editor, and they are how \
agents on a board leave findings for each other and for the user. Call this before \
velm_read_note or velm_write_note rather than guessing a path — a path that does not exist is \
a refusal, not a new note.\n\
\n\
Takes no arguments.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "velm_read_note",
            "description": "\
Read one note and get its markdown back verbatim.\n\
\n\
Arguments: `path` is the note's path exactly as velm_list_notes reported it.\n\
\n\
What it refuses: a note that is private to another agent is not readable, and neither is a \
path that names no note on this board. Both come back as a refusal naming the path — use \
velm_list_notes to see what is actually there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The note's path, as velm_list_notes reports it."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_read_note_chain",
            "description": "\
Read a note **and everything it links to**, following the markdown links from note to note \
and returning the whole trail in one call.\n\
\n\
Use this instead of reading notes one at a time when a note points at others: the board's \
memory is written as a chain on purpose, and this is how you follow it without spending a \
turn per hop or looping for ever on two notes that point at each other.\n\
\n\
Arguments: `path` is the note to start from, as velm_list_notes reports it. `depth` is how \
many hops out to follow; leave it out for the default, and note that asking for more than \
the default does not get you more.\n\
\n\
What it refuses: a starting note that is private to another agent. A note *reached* by a \
link that you may not read is quietly left out of the trail rather than refusing the whole \
call, and a link to a note nobody has written yet is not an error — it is simply not there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The note to start from, as velm_list_notes reports it."
                    },
                    "depth": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How many hops of links to follow. Optional."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_ingest_context",
            "description": "\
Have Velm read a document, a page or a media file as context for you, and keep it.\n\
\n\
Use this for a source you will need again — a specification, an RFC, a long article. The \
text is extracted, stored against your node, and put in front of you at the start of your \
next session, so you do not have to re-read or re-fetch it. For a file you can simply open \
and are done with, open it yourself; this is for context that should persist.\n\
\n\
Arguments: `source` is a path on disk or a URL. PDFs, Word documents, spreadsheets, plain \
text, web pages and YouTube links are understood.\n\
\n\
What it refuses, or reports honestly: a file it cannot reach, a format needing a converter \
this machine has not got, and audio or video — which are attached by path, because Velm \
does not transcribe media. The answer says which of those happened; it is never a silent \
success.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "A path on disk, or a URL."
                    }
                },
                "required": ["source"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_write_note",
            "description": "\
Write to a note. Returns confirmation once it is on disk.\n\
\n\
Arguments: `path` is the note's path, as velm_list_notes reports it. `text` is the markdown \
to write. `append` decides which of two very different things happens: false (the default) \
**replaces the whole note**, and true adds `text` to the end. Append when you are adding a \
finding to a running log; replace only when you mean to discard what is there.\n\
\n\
The user may have the same file open in an editor, so prefer appending, and read the note \
first if you are about to replace it.\n\
\n\
What it refuses: a note private to another agent, and a path that names no note on this \
board.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The note's path." },
                    "text": { "type": "string", "description": "The markdown to write." },
                    "append": {
                        "type": "boolean",
                        "description": "Add to the end instead of replacing. Defaults to false."
                    }
                },
                "required": ["path", "text"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_post_image",
            "description": "\
Put a picture into your own transcript on the board, where the user sees it inline at full \
size — a chart you generated, a screenshot, a diagram. This is worth using: the board is a \
visual workspace and a picture shown is better than a picture described.\n\
\n\
Arguments: supply exactly one of `path` (a file on this machine — PNG, JPEG, GIF or WebP) or \
`data` (the same file's bytes as standard base64). `caption` is optional text shown with it.\n\
\n\
What it refuses: both `path` and `data` together, neither of them, a file that cannot be \
read, and anything that is not a decodable image.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to an image file on this machine."
                    },
                    "data": {
                        "type": "string",
                        "description": "The image file's bytes, standard base64."
                    },
                    "caption": { "type": "string", "description": "Optional caption." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_post_options",
            "description": "\
Offer the user a set of choices as a row of cards on your node, instead of describing the \
options in prose. Clicking one sends that choice back to you as your next turn's input, so \
this is how you ask a question and get a definite answer rather than a paragraph.\n\
\n\
Use it whenever you would otherwise write 'would you like A, B or C?'. It is the one thing a \
chat window cannot do: each card can carry a picture.\n\
\n\
Arguments: `prompt` is the question. `choices` is a list of 2 to 4 cards, each with a short \
`id` you will get back verbatim, a `title` that fits on a card, and optionally a `body` of a \
sentence or two. `image` on a card is a content hash of a picture already in Velm's blob \
store — leave it out unless you were given one.\n\
\n\
Returns once the options are on the board; the user's answer arrives as your next turn, not \
as this tool's result. To put a picture on a card, post it with velm_post_image first and \
pass the hash it answers with as that choice's `image`.\n\
\n\
What it refuses: fewer than two cards, because one option is not a choice, and more than \
four, because they are drawn in one row on the node and a fifth could not be clicked.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The question being asked." },
                    "choices": {
                        "type": "array",
                        "minItems": crate::transcript::MIN_CHOICES,
                        "maxItems": crate::transcript::MAX_CHOICES,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Returned verbatim when this one is picked."
                                },
                                "title": { "type": "string", "description": "The card's heading." },
                                "body": { "type": "string", "description": "A sentence or two." },
                                "image": {
                                    "type": "string",
                                    "description": "A blob hash, if you were given one."
                                }
                            },
                            "required": ["id", "title"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["prompt", "choices"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_spawn_agent",
            "description": "\
Create another agent node on the board and optionally give it its first instruction. Returns \
the new agent's node id, which you can then use with velm_send_message.\n\
\n\
Arguments: `label` is the name written on the node — give it a real one, because five nodes \
called nothing is a board nobody can read. `role` is `worker` (the default and almost always \
right), `orchestrator` or `meta`. `prompt` is the first thing to ask it. `x` and `y` place it \
in board coordinates; leave them out and Velm puts it somewhere sensible, which is the safer \
choice.\n\
\n\
What it refuses, before you try: **only an orchestrator or the meta agent may spawn at all** \
— a worker asking for this is refused outright. An orchestrator also has a hard limit on how \
many agents it may have at once and a region of the board it must stay inside; exceeding \
either is refused with a reason. These limits are enforced by Velm, not by instructions, so \
there is nothing to negotiate: when you are refused, do the work yourself or ask the user to \
raise the limit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "The name on the new node." },
                    "role": {
                        "type": "string",
                        "enum": ["worker", "orchestrator", "meta"],
                        "description": "What the new agent is. Defaults to worker."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The first thing to ask it."
                    },
                    "x": { "type": "number", "description": "Board x, if you must choose." },
                    "y": { "type": "number", "description": "Board y, if you must choose." }
                },
                "required": ["label"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "velm_configure_agent",
            "description": "\
Read, or replace, another agent node's configuration — its role, its provider and model, its \
working directory, its rules, its schedule and its limits. With `model` left out this reads \
and returns the node's configuration as JSON. With `model` supplied it replaces the whole \
configuration with what you give.\n\
\n\
It is a whole-object replace, not a patch, so the only safe way to change one field is: call \
this without `model` to read the current configuration, change the one field in what you got \
back, and call it again passing the entire object.\n\
\n\
Arguments: `node` is the target node's id. `model` is a complete agent configuration object, \
in the same shape this tool returns when reading.\n\
\n\
What it refuses: **only the meta agent may call this at all.** A worker or an orchestrator is \
refused outright, and the refusal is enforced by Velm rather than by instruction. Say what \
you would have changed and let the user do it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node": { "type": "string", "description": "The target node's id." },
                    "model": {
                        "type": "object",
                        "description": "A whole configuration object. Omit to read instead.",
                        "additionalProperties": true
                    }
                },
                "required": ["node"],
                "additionalProperties": false
            }
        }),
    ]
}

// ---------------------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------------------

/// One protocol failure, on its way to a JSON-RPC error object.
struct Failure {
    code: i64,
    message: String,
}

impl Failure {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// Velm's MCP server: a research client, an endpoint, and a line-in/line-out dispatcher.
pub struct Server {
    research: Research,
    endpoint: Endpoint,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// The shipping configuration: research from the environment, Velm from
    /// [`Endpoint::from_env`].
    ///
    /// `ResearchConfig::from_env` rather than the search field alone — it reads the host
    /// policy's own switch too, which for as long as this line named one field was the reason
    /// `allow_local_hosts` could not be turned on by anyone but a recompile.
    pub fn new() -> Self {
        Self::with(Research::new(ResearchConfig::from_env()), Endpoint::from_env())
    }

    /// Both halves supplied. What the tests use, so a protocol assertion cannot depend on the
    /// environment of the machine running it.
    pub fn with(research: Research, endpoint: Endpoint) -> Self {
        Self { research, endpoint }
    }

    /// Handles one line of the stdio session.
    ///
    /// `None` means *say nothing*, which is the correct answer to a notification and the
    /// correct answer to a blank line. Everything else answers with exactly one line.
    ///
    /// # The distinction that decides whether a client survives a bad call
    ///
    /// A **protocol** error — a method that does not exist, a `tools/call` naming a tool that
    /// does not exist, a line that is not JSON — comes back as a JSON-RPC `error`. Clients
    /// treat those as their own fault and some abort the session over one.
    ///
    /// A **tool** failure — a site answered 403, a note is private, an orchestrator is at its
    /// cap — comes back as a *successful* `tools/call` result carrying `isError: true` and the
    /// refusal as text. That is the form a model reads and adapts to. Reporting a 403 as a
    /// JSON-RPC error would tell the client that Velm is broken, when what happened is that
    /// Amazon said no.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let message: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                // The id is unknowable, so it is null — which is what JSON-RPC 2.0 asks for
                // and the one case where a null id is correct rather than a client's mistake.
                return Some(error_frame(
                    Value::Null,
                    code::PARSE_ERROR,
                    &format!("that was not valid JSON: {error}"),
                ));
            }
        };

        let Some(object) = message.as_object() else {
            return Some(error_frame(
                Value::Null,
                code::INVALID_REQUEST,
                "expected a single JSON-RPC object per line; batches are not supported",
            ));
        };

        let method = object.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let id = object.get("id").cloned();

        // ⚠ **The absence of an `id` is the deciding test; the prefix is only a secondary
        // condition on it.** JSON-RPC 2.0 defines a notification as a request *without an
        // `id`*, and nothing else — the method name has no say in it. Reading the two as
        // independent alternatives meant `{"id": 7, "method": "notifications/message"}` was
        // dropped on the floor: no result, no error, and a client blocked on id 7 for as long
        // as it was willing to wait, which for most of them is forever.
        //
        // A null `id` counts as absent, which is what tolerates the client that sends
        // `{"id": null}` alongside a `notifications/…` method: the spec says an id must not
        // be null in a request, so there is nothing there to answer *to*.
        //
        // **An unrecognised notification is ignored rather than answered** — it means a newer
        // client, not a broken session.
        //
        // ⚠ Behaviour change worth knowing about: `{"id": null, "method": "tools/list"}` is
        // now silence where it used to be answered with a result carrying a null id. A null id
        // is not a request id under the spec, so there was never anything for that client to
        // match the answer against.
        if matches!(id, None | Some(Value::Null)) {
            return None;
        }
        let id = id.unwrap_or(Value::Null);

        let outcome = match method {
            protocol::INITIALIZE => Ok(self.initialize(&params)),
            protocol::PING => Ok(json!({})),
            protocol::TOOLS_LIST => Ok(json!({ "tools": tool_definitions() })),
            protocol::TOOLS_CALL => self.call_tool(&params),
            "" => Err(Failure::new(code::INVALID_REQUEST, "the message named no method")),
            other => Err(Failure::new(
                code::METHOD_NOT_FOUND,
                format!("this server does not implement `{other}`"),
            )),
        };

        Some(match outcome {
            Ok(result) => result_frame(id, result),
            Err(failure) => error_frame(id, failure.code, &failure.message),
        })
    }

    /// The `initialize` result.
    ///
    /// **The client's own `protocolVersion` is echoed back when it sends one.** Answering with
    /// [`protocol::VERSION`] regardless would tell a newer client that this server speaks an
    /// older revision than the one it just opened with, and some will disconnect over exactly
    /// that. The constant is the fallback for a client that names none — which is a client
    /// this build has never met, and the honest thing to tell it is what we were written for.
    fn initialize(&self, params: &Value) -> Value {
        let version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(protocol::VERSION);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "velm", "version": env!("CARGO_PKG_VERSION") },
            "instructions": self.instructions(),
        })
    }

    /// The one paragraph a client shows the model about this server as a whole.
    ///
    /// It says which half is available, because that changes what the model should reach for:
    /// a `velm-mcp` started outside Velm has research and nothing else, and a model that finds
    /// that out by calling `velm_list_notes` has spent a turn learning it.
    fn instructions(&self) -> String {
        let mut text = String::from(
            "Velm is a native infinite-canvas board application. This server gives you two \
             things: web research (research_search, research_fetch), which reads pages \
             honestly and reports a site's refusal as a refusal rather than as an empty \
             result; and the Velm board itself (the velm_* tools) — messaging the other \
             agents the user has connected you to, reading and writing the board's shared \
             markdown notes, and posting pictures and clickable option cards into your own \
             node, which the user sees on the canvas.",
        );
        if let Some(reason) = self.endpoint.why() {
            text.push_str(
                "\n\nThe velm_* tools are NOT available in this session and will refuse: ",
            );
            text.push_str(reason);
        }
        text
    }

    /// `tools/call`: find the tool, run it, wrap the answer.
    fn call_tool(&self, params: &Value) -> Result<Value, Failure> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(Failure::new(code::INVALID_PARAMS, "tools/call needs a `name`"));
        };
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        // An unknown tool is a **protocol** error and lists what does exist: the model asked
        // for something that is not on the menu it was given, and the useful answer is the
        // menu. A tool that ran and refused is the other case entirely — see `handle_line`.
        if !TOOL_NAMES.contains(&name) {
            return Err(Failure::new(
                code::INVALID_PARAMS,
                format!("no tool called `{name}`; this server offers {}", TOOL_NAMES.join(", ")),
            ));
        }

        Ok(match self.run_tool(name, &arguments) {
            Ok(text) => content_frame(&text, false),
            Err(text) => content_frame(&text, true),
        })
    }

    /// Run one tool by name, for a caller that is **not** speaking JSON-RPC.
    ///
    /// The dispatch `call_tool` used to hold inline, lifted so that the two callers share it:
    /// an MCP client over stdio, and `transport/http.rs`'s tool loop, which talks the
    /// provider's own function-calling shape and never sees a JSON-RPC frame. One dispatch, so
    /// an HTTP agent and an MCP agent cannot come to offer different tools or answer a refusal
    /// differently — which is the drift this repository has paid for every time one behaviour
    /// grew a second implementation.
    ///
    /// `Err` is a **tool failure**, not a protocol error: it is the refusal a model reads and
    /// adapts to. The caller decides how to present it; see this type's `handle_line` for why
    /// the two must not be conflated. The unknown-tool check stays in `call_tool`, because for
    /// a function-calling wire an unknown name is answered as a tool result rather than as a
    /// protocol failure.
    pub fn run_tool(&self, name: &str, arguments: &Value) -> Result<String, String> {
        match name {
            "research_search" => self.research_search(arguments),
            "research_fetch" => self.research_fetch(arguments),
            _ if TOOL_NAMES.contains(&name) => self.velm_tool(name, arguments),
            _ => Err(format!(
                "no tool called `{name}`; this server offers {}",
                TOOL_NAMES.join(", ")
            )),
        }
    }

    // -- the research half ---------------------------------------------------------------

    fn research_search(&self, arguments: &Value) -> Result<String, String> {
        let query = string_argument(arguments, "query")?;
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(10, |value| value.clamp(1, 25) as usize);

        match self.research.search(&query, limit) {
            Ok(results) => {
                let mut out = format!("{} result(s) for {query:?}:\n", results.len());
                for (index, result) in results.iter().enumerate() {
                    out.push_str(&format!("\n{}. {}\n   {}\n", index + 1, result.title, result.url));
                    if !result.snippet.is_empty() {
                        out.push_str(&format!("   {}\n", result.snippet));
                    }
                }
                Ok(out)
            }
            Err(error) => Err(refusal(&error)),
        }
    }

    fn research_fetch(&self, arguments: &Value) -> Result<String, String> {
        let url = string_argument(arguments, "url")?;
        match self.research.fetch(&url) {
            Ok(page) => {
                let mut out = String::new();
                if let Some(title) = &page.title {
                    out.push_str(&format!("# {title}\n"));
                }
                out.push_str(&format!("{} — HTTP {}\n", page.url, page.status));
                if page.truncated {
                    out.push_str(
                        "(this page was longer than the tool returns; the text below is cut short)\n",
                    );
                }
                out.push('\n');
                out.push_str(&page.text);
                Ok(out)
            }
            Err(error) => Err(refusal(&error)),
        }
    }

    // -- the Velm half -------------------------------------------------------------------

    /// Turns a `velm_*` call into an [`ipc::Request`] and renders what came back.
    fn velm_tool(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let request = match name {
            "velm_send_message" => ipc::Request::Send {
                to: string_argument(arguments, "to")?,
                text: string_argument(arguments, "text")?,
            },
            "velm_list_notes" => ipc::Request::NoteList,
            "velm_read_note" => ipc::Request::NoteRead { path: string_argument(arguments, "path")? },
            "velm_read_note_chain" => ipc::Request::NoteChain {
                path: string_argument(arguments, "path")?,
                depth: arguments
                    .get("depth")
                    .and_then(Value::as_u64)
                    .and_then(|depth| usize::try_from(depth).ok()),
            },
            "velm_write_note" => ipc::Request::NoteWrite {
                path: string_argument(arguments, "path")?,
                text: string_argument(arguments, "text")?,
                append: arguments.get("append").and_then(Value::as_bool).unwrap_or(false),
            },
            "velm_ingest_context" => ipc::Request::Ingest {
                source: string_argument(arguments, "source")?,
            },
            "velm_post_image" => image_request(arguments)?,
            "velm_post_options" => ipc::Request::Options {
                prompt: string_argument(arguments, "prompt")?,
                choices: choices_from(arguments)?,
            },
            "velm_spawn_agent" => ipc::Request::Spawn(spawn_from(arguments)?),
            "velm_configure_agent" => ipc::Request::Configure {
                node: string_argument(arguments, "node")?,
                model: match arguments.get("model") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(Box::new(
                        serde_json::from_value(value.clone()).map_err(|error| {
                            format!("`model` is not a valid agent configuration: {error}")
                        })?,
                    )),
                },
            },
            other => return Err(format!("`{other}` is not wired up in this build")),
        };

        let verb = request.verb().to_owned();
        match self.endpoint.call(request)? {
            Answer::Done => Ok(format!("Done ({verb}).")),
            Answer::Note { text } => Ok(text),
            Answer::Notes { notes } => {
                if notes.is_empty() {
                    return Ok(
                        "There are no notes on this board yet. The user can add one with the \
                         note tool on the canvas."
                            .to_owned(),
                    );
                }
                let mut out = format!("{} note(s) you can read or write:\n", notes.len());
                for note in &notes {
                    let scope = if note.scope.is_private() { "private" } else { "shared" };
                    out.push_str(&format!("\n- {} [{scope}]", note.path));
                    if !note.title.is_empty() {
                        out.push_str(&format!(" — {}", note.title));
                    }
                }
                out.push('\n');
                Ok(out)
            }
            // The hash, said plainly, because the next call the model makes with it is
            // `velm_post_options` — and a sentence it has to parse a hash out of is a sentence
            // it will get wrong.
            Answer::Stored { blob } => Ok(format!(
                "Stored. Its image hash is {blob} — pass that as a choice's `image` in \
                 velm_post_options to show it on a card."
            )),
            Answer::Chain { notes } => {
                if notes.is_empty() {
                    return Ok("That note is not there, or it is empty.".to_owned());
                }
                // Each note under its own heading, in the order the walk reached them —
                // breadth first, so the note asked for comes first and its direct links
                // before theirs. A model reading this needs to know where one note ends and
                // the next begins, which is the whole reason this is not a concatenation.
                let mut out =
                    format!("{} note(s), following the links from the first:\n", notes.len());
                for (path, text) in &notes {
                    out.push_str(&format!("\n===== {path} =====\n{text}\n"));
                }
                Ok(out)
            }
            Answer::Spawned { agent } => Ok(format!(
                "Created agent `{agent}`. Use that id with velm_send_message to talk to it."
            )),
            Answer::Config { model } => serde_json::to_string_pretty(&model)
                .map_err(|error| format!("that node's configuration could not be rendered: {error}")),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------------------

/// A required string argument, or a sentence naming what is missing.
///
/// The message is what the model reads, so it names the argument rather than saying the call
/// was invalid — a model told *"`url` is required"* fixes it on the next call.
fn string_argument(arguments: &Value, name: &str) -> Result<String, String> {
    match arguments.get(name).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_owned()),
        Some(_) => Err(format!("`{name}` was empty; it needs a value")),
        None => Err(format!("`{name}` is required and was not supplied")),
    }
}

/// `path` or `data`, exactly one.
///
/// Reading the file here rather than making the model base64 it is the difference between a
/// tool an agent uses and one it avoids: agents produce *files*, and a model asked to inline
/// half a megabyte of base64 will either refuse or truncate it. `velm-mcp` runs as the agent's
/// own subprocess, so it can already read what the agent can read — no privilege is added by
/// doing it here.
fn image_request(arguments: &Value) -> Result<ipc::Request, String> {
    let caption = arguments
        .get("caption")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty());
    let path = arguments.get("path").and_then(Value::as_str).filter(|text| !text.is_empty());
    let data = arguments.get("data").and_then(Value::as_str).filter(|text| !text.is_empty());

    match (path, data) {
        (Some(path), None) => {
            let bytes = std::fs::read(path)
                .map_err(|error| format!("{path} could not be read: {error}"))?;
            Ok(ipc::Request::Image { data: ipc::encode_base64(&bytes), caption })
        }
        (None, Some(data)) => Ok(ipc::Request::Image { data: data.to_owned(), caption }),
        (Some(_), Some(_)) => Err(
            "supply either `path` or `data`, not both — they are two ways of giving the same \
             picture and there is no way to tell which one you meant"
                .to_owned(),
        ),
        (None, None) => Err(
            "supply either `path` (a file on this machine) or `data` (the file's bytes as \
             base64)"
                .to_owned(),
        ),
    }
}

/// The option cards. Bounded here as well as in the schema, because a schema is advice to the
/// model and this is the thing that actually runs.
fn choices_from(arguments: &Value) -> Result<Vec<crate::transcript::Choice>, String> {
    let Some(array) = arguments.get("choices").and_then(Value::as_array) else {
        return Err("`choices` is required and must be a list of cards".to_owned());
    };
    if array.len() < crate::transcript::MIN_CHOICES {
        return Err("`choices` needs at least two cards — one option is not a choice".to_owned());
    }
    // The painter's row, not a number chosen here. A card past this one is drawn as a line of
    // text saying it exists, which the user cannot click — see `transcript::MAX_CHOICES`.
    if array.len() > crate::transcript::MAX_CHOICES {
        return Err(format!(
            "`choices` takes at most {} cards; they are drawn in one row on the node and a \
             card past that could not be clicked",
            crate::transcript::MAX_CHOICES
        ));
    }
    array
        .iter()
        .map(|entry| {
            let id = string_argument(entry, "id")?;
            let title = string_argument(entry, "title")?;
            let mut choice = crate::transcript::Choice::new(id, title);
            choice.body = entry.get("body").and_then(Value::as_str).map(str::to_owned);
            choice.image = entry.get("image").and_then(Value::as_str).map(str::to_owned);
            Ok(choice)
        })
        .collect()
}

fn spawn_from(arguments: &Value) -> Result<ipc::SpawnRequest, String> {
    let label = string_argument(arguments, "label")?;
    let role = match arguments.get("role").and_then(Value::as_str) {
        None | Some("worker") => crate::model::RoleKind::Worker,
        Some("orchestrator") => crate::model::RoleKind::Orchestrator,
        Some("meta") => crate::model::RoleKind::Meta,
        Some(other) => {
            return Err(format!("`role` must be worker, orchestrator or meta, not `{other}`"));
        }
    };
    let prompt = arguments
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty());
    // Both or neither. One coordinate is a placement nobody meant, and guessing the other from
    // the territory would put the node somewhere the caller did not ask for while looking as
    // though it had been placed deliberately.
    let at = match (
        arguments.get("x").and_then(Value::as_f64),
        arguments.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => Some((x, y)),
        (None, None) => None,
        _ => return Err("give both `x` and `y`, or neither".to_owned()),
    };
    Ok(ipc::SpawnRequest { label, role, prompt, at })
}

/// A research failure as the sentence a model reads.
///
/// It is the error's own `Display`, which already names the status code or the exact robots
/// rule — see [`ResearchError`]. Wrapping it in *"the tool failed"* would bury the one fact
/// worth having.
fn refusal(error: &ResearchError) -> String {
    error.to_string()
}

// ---------------------------------------------------------------------------------------
// JSON-RPC frames
// ---------------------------------------------------------------------------------------

fn result_frame(id: Value, result: Value) -> String {
    frame(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error_frame(id: Value, code: i64, message: &str) -> String {
    frame(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

fn content_frame(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Serialises one frame.
///
/// The fallback is deliberately a *valid frame* rather than a panic or an empty string:
/// serialising a `Value` that was built from `json!` cannot realistically fail, and if it ever
/// did, killing the session would be a far worse answer than one internal error the client can
/// report.
fn frame(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| {
        String::from(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"velm-mcp could not serialise its answer"}}"#,
        )
    })
}

// ---------------------------------------------------------------------------------------
// Tests — protocol only, and never the network
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A server whose Velm half is known-absent, so an assertion cannot depend on whether the
    /// machine running `cargo test` happens to have `VELM_IPC` in its environment.
    ///
    /// The research half is constructed and never used: no test here calls `research_fetch` or
    /// `research_search`, because `cargo test` must stay offline and deterministic. The live
    /// check for those belongs in an example.
    fn server() -> Server {
        Server::with(
            Research::new(ResearchConfig::default()),
            Endpoint::unavailable("not running inside Velm (this is a test)"),
        )
    }

    fn answer(server: &Server, line: &str) -> Value {
        let text = server.handle_line(line).expect("expected an answer");
        serde_json::from_str(&text).expect("the answer was not JSON")
    }

    /// The handshake, including the part that decides whether a newer client stays connected:
    /// its own `protocolVersion` comes back, not ours.
    #[test]
    fn initialize_answers_and_echoes_the_clients_protocol_version() {
        let server = server();
        let reply = answer(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{}}}"#,
        );
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 1);
        assert_eq!(
            reply["result"]["protocolVersion"], "2099-01-01",
            "a newer client was told this server speaks an older revision"
        );
        assert_eq!(reply["result"]["serverInfo"]["name"], "velm");
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        // And a client that names no version is told what we were written for.
        let bare = answer(&server, r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#);
        assert_eq!(bare["result"]["protocolVersion"], protocol::VERSION);
    }

    /// A session with no Velm behind it says so in the one paragraph the model reads up front,
    /// rather than letting it find out by spending a turn on a tool call.
    #[test]
    fn a_session_outside_velm_says_which_half_is_missing() {
        let reply = answer(&server(), r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let instructions = reply["result"]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("research_fetch"), "{instructions}");
        assert!(instructions.contains("NOT available"), "{instructions}");
        assert!(instructions.contains("this is a test"), "the reason was not carried through");
    }

    /// Every tool has to be *callable* by the model that is shown it, which is a constraint on
    /// the name and not a matter of taste: a provider's tool-name pattern is
    /// `^[a-zA-Z0-9_-]{1,64}$`, so the dotted spelling in `docs/07` would be rejected outright.
    #[test]
    fn every_tool_has_a_callable_name_a_schema_and_a_description_worth_reading() {
        let reply = answer(&server(), r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
        let tools = reply["result"]["tools"].as_array().expect("tools/list returned no array");
        assert_eq!(tools.len(), TOOL_NAMES.len());

        let mut seen: Vec<&str> = Vec::new();
        for tool in tools {
            let name = tool["name"].as_str().expect("a tool with no name");
            assert!(
                name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'),
                "`{name}` cannot be sent to a model as a tool name"
            );
            assert!(name.len() <= 64, "`{name}` is too long to be a tool name");
            assert!(!seen.contains(&name), "`{name}` is listed twice");
            seen.push(name);
            assert!(TOOL_NAMES.contains(&name), "`{name}` is not in TOOL_NAMES");

            // The description is the only documentation the calling model ever gets. A short
            // one is a tool that will be called wrongly.
            let description = tool["description"].as_str().unwrap_or_default();
            assert!(description.len() > 200, "`{name}`'s description is too thin to act on");
            assert_eq!(tool["inputSchema"]["type"], "object", "`{name}` has no object schema");
            assert!(tool["inputSchema"]["properties"].is_object(), "`{name}` has no properties");
        }

        // The two halves of the surface are both present, and the Velm half matches the verbs
        // `ipc::Request` actually has — one enum, one surface.
        assert!(seen.contains(&"research_search") && seen.contains(&"research_fetch"));
        // Counted against `TOOL_NAMES` rather than a literal, which is the assertion this was
        // reaching for: the number is bookkeeping, and a tool added to the list and not to the
        // payload — or the reverse — is the failure worth catching. A hardcoded 8 catches that
        // too and also fails for the harmless reason that a tool was added correctly.
        let velm_tools = TOOL_NAMES.iter().filter(|name| name.starts_with("velm_")).count();
        assert_eq!(seen.iter().filter(|name| name.starts_with("velm_")).count(), velm_tools);
        assert_eq!(seen.len(), TOOL_NAMES.len(), "tools/list and TOOL_NAMES disagree");
    }

    /// Every tool that refuses names *what it refuses*, because a boundary a model discovers
    /// by walking into it is a boundary it then argues with.
    #[test]
    fn a_tool_that_can_refuse_says_so_in_its_own_description() {
        let reply = answer(&server(), r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
        let tools = reply["result"]["tools"].as_array().unwrap();
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or_default();
            if name == "velm_list_notes" {
                continue; // The one verb with nothing to refuse: it lists what you may see.
            }
            let description = tool["description"].as_str().unwrap_or_default();
            assert!(
                description.contains("refuse"),
                "`{name}` never tells the model what it will not do"
            );
        }
    }

    /// The distinction the whole dispatcher turns on. An unknown *tool* is a protocol error
    /// listing the menu; a tool that ran and was refused is a **successful** call carrying
    /// `isError`. Getting this backwards makes a 403 look like a broken server.
    #[test]
    fn an_unknown_tool_errors_and_a_refused_one_succeeds_with_is_error() {
        let server = server();

        let unknown = answer(
            &server,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"research.search","arguments":{}}}"#,
        );
        assert_eq!(unknown["error"]["code"], code::INVALID_PARAMS);
        let message = unknown["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("research_search"), "the menu was not offered: {message}");
        assert!(unknown.get("result").is_none());

        // A real tool with Velm absent: the call succeeds, the *tool* reports the failure.
        let refused = answer(
            &server,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"velm_list_notes","arguments":{}}}"#,
        );
        assert!(refused.get("error").is_none(), "a refusal was reported as a protocol error");
        assert_eq!(refused["result"]["isError"], true);
        let text = refused["result"]["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("this is a test"), "the reason was not passed on: {text}");
        assert_eq!(refused["result"]["content"][0]["type"], "text");
    }

    /// A missing argument is reported as a tool result naming the argument, not as a protocol
    /// error — the model can fix that on its next call, and a protocol error may end the
    /// session before it gets one.
    #[test]
    fn a_missing_argument_names_itself() {
        let reply = answer(
            &server(),
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"velm_read_note","arguments":{}}}"#,
        );
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("`path`"), "{text}");
    }

    /// The two shapes of a bad image call, both refused before anything is sent.
    #[test]
    fn posting_an_image_needs_exactly_one_of_path_and_data() {
        let neither = image_request(&json!({})).unwrap_err();
        assert!(neither.contains("path") && neither.contains("data"), "{neither}");

        let both = image_request(&json!({ "path": "/tmp/a.png", "data": "AAAA" })).unwrap_err();
        assert!(both.contains("not both"), "{both}");

        let inline = image_request(&json!({ "data": "AAAA", "caption": "a chart" })).unwrap();
        assert!(matches!(inline, ipc::Request::Image { ref caption, .. } if caption.as_deref() == Some("a chart")));
    }

    /// One option is not a choice, and more than the painter draws do not fit on a node. Both
    /// bounded here as well as in the schema, because the schema is advice to the model and
    /// this is what runs.
    ///
    /// ⚠ **The upper bound is the painter's row, not a number spelled here.** It used to be
    /// eight, spelled in three places — this validator, the schema's `maxItems`, and the
    /// tool's own description — while `draw::MAX_OPTION_CARDS` drew four and rendered the rest
    /// as a line of text saying how many options the user could not click. The assertion is on
    /// [`crate::transcript::MAX_CHOICES`] rather than on a literal so that moving the row's
    /// capacity moves the refusal with it.
    #[test]
    fn option_cards_are_bounded_and_carry_their_ids() {
        let too_few = choices_from(&json!({ "choices": [{ "id": "a", "title": "A" }] }));
        assert!(too_few.unwrap_err().contains("at least two"));

        let over = crate::transcript::MAX_CHOICES + 1;
        let many: Vec<Value> =
            (0..over).map(|index| json!({ "id": index.to_string(), "title": "x" })).collect();
        let refusal = choices_from(&json!({ "choices": many })).unwrap_err();
        assert!(
            refusal.contains(&crate::transcript::MAX_CHOICES.to_string()),
            "the refusal did not say how many are allowed: {refusal}"
        );
        assert!(
            refusal.contains("clicked"),
            "the refusal did not say why, which is what makes it actionable: {refusal}"
        );

        let good = choices_from(&json!({ "choices": [
            { "id": "warm", "title": "Warm", "body": "Coral and cream." },
            { "id": "cool", "title": "Cool" }
        ] }))
        .unwrap();
        assert_eq!(good.len(), 2);
        assert_eq!(good[0].id, "warm");
        assert_eq!(good[0].body.as_deref(), Some("Coral and cream."));
        assert_eq!(good[1].body, None);
    }

    /// A placement is both coordinates or neither. One is a position nobody meant.
    #[test]
    fn a_spawn_takes_both_coordinates_or_neither() {
        assert!(spawn_from(&json!({ "label": "Builder", "x": 10.0 })).is_err());
        let placed = spawn_from(&json!({ "label": "Builder", "x": 10.0, "y": -4.0 })).unwrap();
        assert_eq!(placed.at, Some((10.0, -4.0)));
        assert_eq!(placed.role, crate::model::RoleKind::Worker, "the default must be a worker");

        let bad_role = spawn_from(&json!({ "label": "B", "role": "wizard" })).unwrap_err();
        assert!(bad_role.contains("worker"), "{bad_role}");
    }

    /// Unknown methods, notifications and garbage — the three ways a session ends badly if any
    /// of them is handled wrongly.
    #[test]
    fn unknown_methods_error_notifications_are_silent_and_garbage_does_not_kill_the_session() {
        let server = server();

        let unknown =
            answer(&server, r#"{"jsonrpc":"2.0","id":7,"method":"resources/list","params":{}}"#);
        assert_eq!(unknown["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(unknown["id"], 7);

        // A notification is never answered — not the one we know, and **not one we do not**,
        // which is the whole tolerance rule: an unrecognised notification means a newer
        // client, not a broken session.
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        assert_eq!(
            server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/from/the/future","params":{"x":1}}"#),
            None
        );
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","method":"tools/list"}"#), None, "no id is a notification");
        assert_eq!(server.handle_line(""), None);
        assert_eq!(server.handle_line("   \n"), None);

        // ⚠ **An `id` means an answer is owed, whatever the method is called.** The prefix
        // used to be an *independent* test, so this message got no result and no error and
        // the client blocked on id 7 — which is the one failure a stdio server must never
        // have, since there is nothing else on the pipe to notice it. A method in that
        // namespace that carries an id is not a notification; it is a request for something
        // this server does not implement, and that has a defined answer.
        let with_id = answer(
            &server,
            r#"{"jsonrpc":"2.0","id":7,"method":"notifications/message","params":{"level":"info"}}"#,
        );
        assert_eq!(with_id["id"], 7, "a request under `notifications/` went unanswered");
        assert_eq!(with_id["error"]["code"], code::METHOD_NOT_FOUND);

        // The same rule for a namespace nobody has invented yet.
        let future = answer(&server, r#"{"jsonrpc":"2.0","id":8,"method":"notifications/x/y"}"#);
        assert_eq!(future["id"], 8);

        // Garbage gets a parse error with a null id, and the server is still usable after it.
        let garbage = answer(&server, "this is not json {");
        assert_eq!(garbage["error"]["code"], code::PARSE_ERROR);
        assert_eq!(garbage["id"], Value::Null);

        // Bytes that are not UTF-8 at all, as `velm-mcp`'s loop now hands them over: read as
        // bytes and decoded lossily, so one stray byte from a client is a **protocol** error
        // with an answer rather than the end of the server. `read_line` refuses the whole read
        // on that byte, and the binary's loop treats an `Err` as the end of stdin — so the
        // session died over one byte, which is precisely what that file's own header says it
        // never does.
        let invalid = String::from_utf8_lossy(b"{\"jsonrpc\":\"2.0\",\"id\":9,\xff\xfe}");
        let lossy = answer(&server, &invalid);
        assert_eq!(lossy["error"]["code"], code::PARSE_ERROR);
        let after = answer(&server, r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#);
        assert_eq!(after["id"], 10, "the server did not survive a line that was not UTF-8");

        let batch = answer(&server, r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
        assert_eq!(batch["error"]["code"], code::INVALID_REQUEST);

        let ping = answer(&server, r#"{"jsonrpc":"2.0","id":8,"method":"ping"}"#);
        assert!(ping["result"].is_object(), "ping must answer with an empty result object");
        assert!(ping.get("error").is_none());
    }

    /// The token in an [`Endpoint`] authorises `velm_spawn` and `velm_configure` — the two
    /// verbs that make new processes and change a node's model — so it is the last thing that
    /// should reach a log. An `Endpoint` is held for the life of the server and is the natural
    /// thing to print when a `velm_*` tool refuses something unexpectedly, which is precisely
    /// the session whose output gets pasted into a bug report.
    ///
    /// Asserted through `Endpoint`, not `Reach`, because that is the shape it would be printed
    /// in: a derived `Debug` prints its fields with *their* impls, so this is the join.
    #[test]
    fn an_endpoint_debug_never_prints_the_token_that_authorises_spawn() {
        let secret = "tok-9c1e-never-print-me";
        let endpoint = Endpoint {
            reach: Ok(Reach {
                address: SocketAddr::from(([127, 0, 0, 1], 51234)),
                token: secret.to_owned(),
                agent: "node-3".to_owned(),
            }),
        };

        let printed = format!("{endpoint:?}");
        assert!(!printed.contains(secret), "the endpoint printed its token: {printed}");
        assert!(!printed.contains("token"), "even the field name invites a second look");
        // Which Velm, and as whom — the question a debug print is actually asked.
        assert!(printed.contains("51234") && printed.contains("node-3"), "{printed}");

        // The unavailable form has no secret in it and must still say why.
        let missing = Endpoint::unavailable("VELM_IPC is not set");
        assert!(format!("{missing:?}").contains("VELM_IPC"));
    }

    /// Every frame is exactly one line. A response carrying a newline splits into two frames on
    /// the wire, and the second one is not JSON — which ends the session.
    #[test]
    fn no_answer_ever_contains_a_newline() {
        let server = server();
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"velm_list_notes"}}"#,
            "not json",
        ] {
            let reply = server.handle_line(line).expect("expected an answer");
            assert!(!reply.contains('\n'), "an answer spanned two frames: {reply}");
        }
    }

    /// The `VELM_IPC` seam, which `velm-agent-cli` has to resolve identically. Both accepted
    /// shapes are pinned here so a change to one of them fails a test rather than silently
    /// diverging from the shim.
    #[test]
    fn the_ipc_address_form_accepts_a_port_or_a_socket_and_nothing_else() {
        assert_eq!(
            parse_address("51234"),
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 51234)))
        );
        assert_eq!(
            parse_address("127.0.0.1:51234"),
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 51234)))
        );
        assert_eq!(parse_address("0"), None, "port zero is not an address to call");
        // A path is not an address, which is what sends it down the runtime-file branch.
        assert_eq!(parse_address("/Users/x/Library/Application Support/Vellum/runtime/ipc.json"), None);
        assert_eq!(parse_address(""), None);
    }

    /// An endpoint that cannot be reached still answers — with a sentence, not a panic.
    #[test]
    fn an_unavailable_endpoint_refuses_by_name() {
        let endpoint = Endpoint::unavailable("VELM_IPC is not set");
        assert!(!endpoint.is_available());
        assert_eq!(endpoint.agent(), None);
        let refused = endpoint.call(ipc::Request::NoteList).unwrap_err();
        assert_eq!(refused, "VELM_IPC is not set");
    }
}
