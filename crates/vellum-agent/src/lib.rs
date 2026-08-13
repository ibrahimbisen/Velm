//! Velm's Agent Canvas layer: everything about running AI agents on a board that does not
//! need a window, a GPU or a document.
//!
//! `docs/07-agent-canvas.md` is the contract this crate implements. Read it first — the
//! rules that govern the joins between these modules are stated there rather than in any
//! one of them.
//!
//! ```text
//!   AgentModel ──► session ──► transport ─┬─ acp   (a CLI that holds the user's subscription)
//!   (in the doc)      │                   ├─ pty   (a real terminal)
//!                     │                   └─ http  (an API, incl. a model on this machine)
//!                     ▼
//!               TranscriptEvent ──► sidecar (JSONL on disk, never in the document)
//!                     │
//!                     └──► bus ──► another agent, along a connector the user drew
//! ```
//!
//! # What this crate deliberately does not do
//!
//! - **It does not depend on anything in the workspace.** Not `vellum-doc`, not `vellum-app`.
//!   An item id reaches it as a string. That is what keeps every rule cascade, every
//!   protocol frame and every schedule an ordinary unit test on a machine with no display.
//! - **It does not measure text or lay anything out.** `vellum-app` does, exactly as it does
//!   for `vellum-flow` and `vellum-mindmap`.
//! - **It does not read the clock.** Every function that cares about time takes it as an
//!   argument, so "does a daily 18:00 job fire at 17:59" is arithmetic rather than a wait.
//! - **It costs nothing when unused.** No thread, no process, no socket and no timer exists
//!   until the user places an agent node. A board without one is unchanged, on disk and per
//!   frame, by this crate's existence.

pub mod bus;
pub mod filetree;
pub mod ingest;
pub mod ipc;
pub mod mcp;
pub mod model;
pub mod notes;
pub mod orchestrator;
pub mod provider;
pub mod research;
pub mod rules;
pub mod schedule;
pub mod session;
pub mod sidecar;
pub mod summary;
pub mod transcript;
pub mod transport;
pub mod voice;
pub mod worktree;

pub use model::{
    AgentModel, AgentRules, BrowserModel, ContextSource, DisplayMode, FileTreeModel, NoteModel,
    NoteScope, RoleKind, Territory,
};
pub use bus::{Bus, Delivery, LinkDirection, LinkPulse, Message, Topology};
pub use ipc::{IpcHandler, IpcServer};
pub use notes::{Freshness, NoteStore, Requester, Save};
pub use orchestrator::{AgentNode, NodeBox, Refusal};
pub use provider::{Provider, ProviderChoice, Transport};
pub use rules::{Layer, Permissions, ResolvedRules, RuleFile, Saved};
pub use session::{Session, Status};
pub use sidecar::{BoardKey, Record, Sidecar, Tail};
pub use summary::{Activity, AgentDigest, Attention, Digest};
pub use schedule::{Completion, Recurrence, Schedule, Timestamp, Trigger};
pub use transport::{AgentTransport, LaunchSpec, PendingBlob};
pub use transcript::{
    AgentRef, Choice, RequestId, ToolCallId, TranscriptEvent, TurnId, TurnOutcome,
};

/// Anything this crate can fail at.
///
/// One error type across the whole crate rather than one per module: every one of these
/// ends up in the same place — a toast, and a `TranscriptEvent::Error` on the node that
/// caused it — so a caller that had to match on six error types would immediately flatten
/// them again.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The agent binary is not installed, or not on `PATH`.
    ///
    /// Its own variant because it is the commonest failure by a wide margin and the one
    /// with a specific remedy: it names the command that was looked for, so the toast can
    /// say *"claude is not installed"* rather than *"failed to start"*.
    #[error("`{command}` is not installed or is not on your PATH")]
    MissingCommand { command: String },

    /// The provider refused the credentials, or there are none.
    #[error("{provider} refused the credentials: {message}")]
    Unauthorized { provider: String, message: String },

    /// The transport died, or spoke something we could not read.
    #[error("the {transport} connection failed: {message}")]
    Transport { transport: &'static str, message: String },

    /// A message was sent to an agent with no connector to it.
    ///
    /// A refusal rather than an implicit link: a connector is how the user says two agents
    /// may talk, and an agent that could message anyone would make the lines on the board
    /// decorative.
    #[error("{from} is not connected to {to}")]
    NotConnected { from: String, to: String },

    /// An orchestrator tried to exceed its cap, or spawn outside its territory.
    #[error("{0}")]
    Refused(String),

    /// A note, a working directory or a context file could not be read or written.
    #[error("{path}: {message}")]
    File { path: String, message: String },

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, AgentError>;

impl AgentError {
    /// A file error that names its path, which is the only form of these worth showing.
    pub fn file(path: impl Into<String>, error: &std::io::Error) -> Self {
        Self::File { path: path.into(), message: error.to_string() }
    }

    /// Whether retrying could plausibly work.
    ///
    /// Used to decide between *"try again"* and *"fix this first"* on the node. A missing
    /// binary and a refused credential are not worth retrying; a dropped connection is.
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transport { .. } | Self::Io(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error's text is what a user sees on the node, so it has to name the thing they
    /// can act on — the command, the provider, the path.
    #[test]
    fn errors_name_the_thing_that_can_be_fixed() {
        let missing = AgentError::MissingCommand { command: "claude".into() };
        assert!(missing.to_string().contains("claude"), "{missing}");
        assert!(!missing.is_transient(), "a missing binary is not worth retrying");

        let refused = AgentError::Unauthorized {
            provider: "Kimi".into(),
            message: "invalid key".into(),
        };
        assert!(refused.to_string().contains("Kimi"));
        assert!(!refused.is_transient());

        let dropped =
            AgentError::Transport { transport: "acp", message: "pipe closed".into() };
        assert!(dropped.is_transient(), "a dropped connection is worth retrying");

        let unlinked =
            AgentError::NotConnected { from: "Planner".into(), to: "Builder".into() };
        assert!(unlinked.to_string().contains("Planner"));
        assert!(unlinked.to_string().contains("Builder"));
    }
}
