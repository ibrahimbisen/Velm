//! What an agent said, and what it did to say it.
//!
//! Every transport — ACP, PTY, direct HTTP — produces exactly this stream. That is the
//! point of the module: the painter, the two display modes, the summary writer and the
//! away-mode digest all read [`TranscriptEvent`], and none of them can tell which kind of
//! process produced it. Adding a fourth transport is a new producer and no new consumer.

use serde::{Deserialize, Serialize};

/// One turn: everything between a prompt and the agent's answer to it.
///
/// Monotonic per session and stable across a restart, because it is the key a transcript
/// on disk is read back by. A `u64` rather than a UUID: a transcript is per node, so it
/// only has to be unique within one file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct TurnId(pub u64);

/// One tool call within a turn, so a result can be joined to the call that asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

/// A permission request awaiting the user's answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub String);

/// Who a message came from, in a form that survives a restart.
///
/// The item id is the board's, rendered as its string form — this crate does not depend
/// on `vellum-doc` and must not, so it holds the identifier rather than the type. `name`
/// is carried alongside because a transcript is read long after the node it names may have
/// been deleted, and *"from 42@7"* is not something a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentRef {
    pub item: String,
    pub name: String,
}

impl AgentRef {
    pub fn new(item: impl Into<String>, name: impl Into<String>) -> Self {
        Self { item: item.into(), name: name.into() }
    }
}

/// One selectable option an agent offered.
///
/// The pattern this exists for: *"here are three UI directions I built"* — a row of cards
/// the user clicks one of, rather than three paragraphs describing pictures. `image` is a
/// content hash into the **existing** blob store, addressed exactly as a pasted screenshot
/// is, so deduplication and the texture-residency budget come for free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// Stable within the turn; sent back verbatim when the user picks this one.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// BLAKE3 content hash of a picture in the shared blob store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

impl Choice {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self { id: id.into(), title: title.into(), body: None, image: None }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn with_image(mut self, blob: impl Into<String>) -> Self {
        self.image = Some(blob.into());
        self
    }
}

/// How a turn finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum TurnOutcome {
    /// The agent answered and is idle again.
    Completed,
    /// The user stopped it.
    Cancelled,
    /// The agent gave up, or the transport died. The message is shown on the node.
    Failed { message: String },
    /// The agent hit its own limit — context, tokens, a tool budget — and stopped short.
    Exhausted { message: String },
}

/// One thing that happened, in the order it happened.
///
/// # Why the raw/clean split is a property of the event and not of the renderer
///
/// [`TranscriptEvent::visible_in_clean_mode`] is the single definition of which of these a
/// user sees in Clean mode. Putting it here rather than in the painter is what stops the
/// canvas, the away-mode summary and any future export from each having their own opinion
/// about what "the polished output" means and quietly disagreeing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum TranscriptEvent {
    TurnStarted { turn: TurnId, prompt: String },
    /// The agent's intermediate reasoning. Raw mode only.
    Thought { text: String },
    /// A tool the agent invoked — a shell command, a file read, an edit. Raw mode only.
    ToolCall { id: ToolCallId, name: String, input: String },
    /// What the tool answered. Raw mode only.
    ToolResult { id: ToolCallId, output: String, ok: bool },
    /// Prose the agent addressed to the user. Both modes — this *is* the answer.
    Text { text: String },
    /// A picture, by content hash into the shared blob store. Both modes.
    Image { blob: String, caption: Option<String> },
    /// A set of options to pick between. Both modes. See [`Choice`].
    Options { prompt: String, choices: Vec<Choice>, chosen: Option<String> },
    /// The agent wants permission to do something. Both modes, always: a question the
    /// user must answer cannot be hidden by a display preference.
    PermissionRequest { id: RequestId, summary: String, detail: String },
    /// The user's answer to a [`TranscriptEvent::PermissionRequest`].
    PermissionAnswer { id: RequestId, allowed: bool },
    /// A message that arrived from another agent along a connector. Both modes: it is not
    /// this agent's scaffolding, it is another agent talking, and hiding it would make a
    /// wired-up board impossible to follow.
    Message { from: AgentRef, text: String },
    /// A message this agent sent to another.
    MessageSent { to: AgentRef, text: String },
    /// Raw terminal bytes, for a PTY session shown as a terminal. Raw mode only.
    Terminal { text: String },
    TurnEnded { turn: TurnId, outcome: TurnOutcome },
    /// Something went wrong that is not a turn's own failure — the transport dropped, the
    /// binary is missing, the credentials were refused. Both modes.
    Error { message: String },
}

impl TranscriptEvent {
    /// Whether Clean mode shows this event.
    ///
    /// Clean mode is *"the answer, not the scaffolding"* — the claude.ai experience rather
    /// than the VS Code extension's. So prose, pictures, options, questions, errors and
    /// inter-agent traffic survive; thoughts, tool calls, tool results and raw terminal
    /// bytes do not.
    ///
    /// Two are deliberately visible in both modes despite being scaffolding-shaped.
    /// **A permission request** is a question only the user can answer, and a mode that
    /// hid it would deadlock the agent behind a preference. **An error** is the one thing
    /// a user in Clean mode most needs, because Clean mode is exactly where a silent
    /// failure looks like an agent that simply never replied.
    pub const fn visible_in_clean_mode(&self) -> bool {
        match self {
            Self::Text { .. }
            | Self::Image { .. }
            | Self::Options { .. }
            | Self::PermissionRequest { .. }
            | Self::PermissionAnswer { .. }
            | Self::Message { .. }
            | Self::MessageSent { .. }
            | Self::Error { .. }
            | Self::TurnStarted { .. }
            | Self::TurnEnded { .. } => true,
            Self::Thought { .. }
            | Self::ToolCall { .. }
            | Self::ToolResult { .. }
            | Self::Terminal { .. } => false,
        }
    }

    /// A one-line label for this event, for the away-mode digest and the node's status row.
    pub fn headline(&self) -> String {
        match self {
            Self::TurnStarted { prompt, .. } => format!("asked: {}", first_line(prompt)),
            Self::Thought { text } => format!("thinking: {}", first_line(text)),
            Self::ToolCall { name, .. } => format!("ran {name}"),
            Self::ToolResult { ok: true, .. } => "tool succeeded".into(),
            Self::ToolResult { ok: false, .. } => "tool failed".into(),
            Self::Text { text } => first_line(text),
            Self::Image { caption, .. } => {
                caption.as_deref().map_or_else(|| "sent a picture".into(), first_line)
            }
            Self::Options { prompt, choices, .. } => {
                format!("offered {} options: {}", choices.len(), first_line(prompt))
            }
            Self::PermissionRequest { summary, .. } => format!("asked permission: {summary}"),
            Self::PermissionAnswer { allowed: true, .. } => "permission granted".into(),
            Self::PermissionAnswer { allowed: false, .. } => "permission refused".into(),
            Self::Message { from, text } => format!("{} said: {}", from.name, first_line(text)),
            Self::MessageSent { to, text } => format!("told {}: {}", to.name, first_line(text)),
            Self::Terminal { .. } => "terminal output".into(),
            Self::TurnEnded { outcome, .. } => match outcome {
                TurnOutcome::Completed => "finished".into(),
                TurnOutcome::Cancelled => "stopped".into(),
                TurnOutcome::Failed { message } => format!("failed: {message}"),
                TurnOutcome::Exhausted { message } => format!("ran out: {message}"),
            },
            Self::Error { message } => format!("error: {message}"),
        }
    }
}

/// The first line, trimmed and bounded, for a one-line label.
///
/// Bounded by **characters, not bytes** — `text[..80]` panics on any multi-byte character
/// straddling the boundary, and `strip_site_affix` already aborted the whole process twice
/// that way (feedback 30). A headline is built from arbitrary agent output, which is the
/// least controlled string in the application.
fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(80).collect();
    if out.chars().count() < line.chars().count() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_mode_hides_the_scaffolding_and_keeps_the_answer() {
        let hidden = [
            TranscriptEvent::Thought { text: "hmm".into() },
            TranscriptEvent::ToolCall {
                id: ToolCallId("1".into()),
                name: "bash".into(),
                input: "ls".into(),
            },
            TranscriptEvent::ToolResult {
                id: ToolCallId("1".into()),
                output: "a b".into(),
                ok: true,
            },
            TranscriptEvent::Terminal { text: "\u{1b}[0m".into() },
        ];
        for event in &hidden {
            assert!(!event.visible_in_clean_mode(), "{event:?} leaked into clean mode");
        }

        let shown = [
            TranscriptEvent::Text { text: "the answer".into() },
            TranscriptEvent::Image { blob: "abc".into(), caption: None },
            TranscriptEvent::Options {
                prompt: "pick".into(),
                choices: vec![Choice::new("a", "A")],
                chosen: None,
            },
            TranscriptEvent::Message {
                from: AgentRef::new("1@2", "Reviewer"),
                text: "done".into(),
            },
        ];
        for event in &shown {
            assert!(event.visible_in_clean_mode(), "{event:?} was hidden in clean mode");
        }
    }

    /// A question the user must answer, and a failure they must see, cannot be hidden by a
    /// display preference — the first would deadlock the agent behind a setting, and the
    /// second is exactly what makes a silent failure look like an agent that never replied.
    #[test]
    fn clean_mode_never_hides_a_question_or_a_failure() {
        let asking = TranscriptEvent::PermissionRequest {
            id: RequestId("r1".into()),
            summary: "write to src/main.rs".into(),
            detail: String::new(),
        };
        assert!(asking.visible_in_clean_mode());

        let broken = TranscriptEvent::Error { message: "claude: command not found".into() };
        assert!(broken.visible_in_clean_mode());
    }

    /// The transcript is JSONL on disk, so every event has to survive a round trip through
    /// a single line of JSON. A tag that changes is a transcript that cannot be read back.
    #[test]
    fn every_event_round_trips_through_one_line_of_json() {
        let events = vec![
            TranscriptEvent::TurnStarted { turn: TurnId(1), prompt: "go".into() },
            TranscriptEvent::Thought { text: "considering".into() },
            TranscriptEvent::ToolCall {
                id: ToolCallId("t".into()),
                name: "read".into(),
                input: "{}".into(),
            },
            TranscriptEvent::ToolResult {
                id: ToolCallId("t".into()),
                output: "ok".into(),
                ok: true,
            },
            TranscriptEvent::Text { text: "hello\nworld".into() },
            TranscriptEvent::Image { blob: "hash".into(), caption: Some("a chart".into()) },
            TranscriptEvent::Options {
                prompt: "which".into(),
                choices: vec![Choice::new("1", "One").with_image("h")],
                chosen: Some("1".into()),
            },
            TranscriptEvent::PermissionRequest {
                id: RequestId("p".into()),
                summary: "s".into(),
                detail: "d".into(),
            },
            TranscriptEvent::PermissionAnswer { id: RequestId("p".into()), allowed: true },
            TranscriptEvent::Message {
                from: AgentRef::new("1@2", "Planner"),
                text: "take this".into(),
            },
            TranscriptEvent::MessageSent {
                to: AgentRef::new("3@4", "Builder"),
                text: "done".into(),
            },
            TranscriptEvent::Terminal { text: "$ ls".into() },
            TranscriptEvent::TurnEnded { turn: TurnId(1), outcome: TurnOutcome::Completed },
            TranscriptEvent::Error { message: "boom".into() },
        ];

        for event in events {
            let line = serde_json::to_string(&event).expect("serialise");
            assert!(!line.contains('\n'), "an event serialised across two JSONL lines: {line}");
            let back: TranscriptEvent = serde_json::from_str(&line).expect("deserialise");
            assert_eq!(back, event);
        }
    }

    /// A headline is built from arbitrary agent output — the least controlled string in the
    /// application — so it must not be able to panic. `strip_site_affix` aborted the whole
    /// process twice on exactly this shape of bug (feedback 30), and `panic = "abort"` in
    /// the release profile means there is no catching it.
    #[test]
    fn a_headline_never_panics_on_multibyte_or_empty_text() {
        let long_cjk = "夕".repeat(400);
        let cases = [
            String::new(),
            "   ".into(),
            "\n\n".into(),
            long_cjk,
            "café".repeat(30),
            "🙂".repeat(200),
        ];
        for text in cases {
            let headline = TranscriptEvent::Text { text: text.clone() }.headline();
            assert!(headline.chars().count() <= 81, "{headline}");
        }
    }

    #[test]
    fn a_headline_takes_the_first_line_and_marks_what_it_cut() {
        let event = TranscriptEvent::Text { text: "first line\nsecond line".into() };
        assert_eq!(event.headline(), "first line");

        let long = TranscriptEvent::Text { text: "x".repeat(200) };
        assert!(long.headline().ends_with('…'));
    }
}
