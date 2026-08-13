//! The running-agent state machine: idle → running → idle, with the awkward parts.
//!
//! One [`Session`] is one live agent node. It owns the transport, allocates turn ids, holds
//! the prompts that arrived while it was busy, and answers the two questions the painter
//! asks every frame — *what is this agent doing* and *what has it said since I last looked*.
//!
//! ```text
//!            prompt()                      TurnEnded
//!   Idle ─────────────► Running ──────────────────────► Idle
//!     ▲                    │  ▲                           │
//!     │                    │  └── PermissionAnswer ───────┤
//!     │       PermissionRequest ─► WaitingForPermission ──┘
//!     └──────────────── TurnStarted (a new turn clears an error) ── Error
//! ```
//!
//! # Draining is pull-based, and nothing here ever blocks
//!
//! [`Session::poll`] is called once per frame by `vellum-app`'s agent runtime — the shape
//! `links.rs` already uses for link fetches — and returns only what has arrived. It uses
//! `try_recv`, never `recv`, and it is **bounded**: at most [`POLL_BUDGET`] events leave per
//! call, because an agent printing a build log faster than the display refreshes would
//! otherwise turn one frame into an unbounded amount of work. What is left over is still in
//! the channel and comes out next frame.
//!
//! # A prompt that arrives mid-turn queues; it does not interleave
//!
//! Two prompts in one turn is not a thing any of the three transports can express, and
//! sending one anyway produces an agent answering a question it has already been asked half
//! of. So a prompt arriving while a turn is in flight waits, and the queue drains in
//! [`Session::poll`] as each turn ends.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crate::provider::Transport as TransportKind;
use crate::transcript::{RequestId, TranscriptEvent, TurnId, TurnOutcome};
use crate::transport::{self, AgentTransport, LaunchSpec, PendingBlob};
use crate::{AgentError, Result};

/// The most events one [`Session::poll`] hands back.
///
/// A frame budget, not a capacity: a chatty agent's remaining output is still in the channel
/// and arrives next frame. 512 is far more than a person can read in 16ms and far less than
/// a `cargo build` produces in a second.
pub const POLL_BUDGET: usize = 512;

/// The most prompts that may be waiting behind the turn in flight.
///
/// An orchestrator in a loop can queue faster than a worker can answer, and an unbounded
/// queue is a board that appears to hang while working through hours of stale instructions.
/// Refused with a message rather than silently dropped — see `docs/07` §9's rule that a
/// limit which only lives in a prompt is not a limit.
pub const MAX_QUEUED: usize = 32;

/// What the node shows.
///
/// Four states rather than a boolean, because the painter draws each one differently and
/// two of them are *waiting for the user* rather than waiting for the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Nothing in flight. The ordinary state, and the one a board full of agents sits in.
    #[default]
    Idle,
    /// A turn is running.
    Running,
    /// The agent asked permission and **is blocked until the user answers**. Distinct from
    /// `Running` because the thing that unblocks it is a click, not patience.
    WaitingForPermission,
    /// Something went wrong. The detail line says what.
    Error,
}

impl Status {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Working",
            Self::WaitingForPermission => "Needs you",
            Self::Error => "Failed",
        }
    }

    /// Whether this agent is waiting on the *user* rather than on itself. What the board
    /// uses to draw attention to the handful of nodes that need a person.
    pub const fn needs_attention(self) -> bool {
        matches!(self, Self::WaitingForPermission | Self::Error)
    }

    /// Whether a turn is in flight, either working or blocked on a question.
    pub const fn is_busy(self) -> bool {
        matches!(self, Self::Running | Self::WaitingForPermission)
    }
}

/// One live agent.
pub struct Session {
    transport: Box<dyn AgentTransport>,
    events: Receiver<TranscriptEvent>,
    /// The session's own way into the stream, for the failures it discovers itself — a
    /// prompt that could not be sent is a thing that happened, and a transcript that omitted
    /// it would show a question the agent simply never answered.
    voice: Sender<TranscriptEvent>,
    status: Status,
    detail: String,
    current: Option<TurnId>,
    next_turn: u64,
    queue: VecDeque<String>,
    outstanding: Vec<RequestId>,
    system_context: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("status", &self.status)
            .field("detail", &self.detail)
            .field("turn", &self.current)
            .field("queued", &self.queue.len())
            .field("awaiting_permission", &self.outstanding.len())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Starts the transport this spec asks for.
    ///
    /// Costs a process for ACP and PTY and **nothing at all** for HTTP, which is the "no
    /// idle cost" rule (`docs/07` §0) as far down as it goes: placing a node starts a
    /// session, and a session that is never prompted never speaks to anyone.
    pub fn start(spec: &LaunchSpec) -> Result<Self> {
        let (voice, events) = channel();
        let transport = transport::start(spec, voice.clone())?;
        Ok(Self::over(transport, voice, events, spec.system_context.clone()))
    }

    /// Wraps a transport the caller built.
    ///
    /// The seam that makes the state machine testable: every rule below — queueing, the
    /// permission state, the turn ids — is exercised against a transport that does nothing
    /// but record what it was asked, on a machine with no agent installed.
    pub fn over(
        transport: Box<dyn AgentTransport>,
        voice: Sender<TranscriptEvent>,
        events: Receiver<TranscriptEvent>,
        system_context: String,
    ) -> Self {
        Self {
            transport,
            events,
            voice,
            status: Status::Idle,
            detail: String::new(),
            current: None,
            next_turn: 1,
            queue: VecDeque::new(),
            outstanding: Vec::new(),
            system_context,
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// A line a person can read: what it is working on, what it is asking, or what failed.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub fn kind(&self) -> TransportKind {
        self.transport.kind()
    }

    /// The turn in flight, if there is one.
    pub fn current_turn(&self) -> Option<TurnId> {
        self.current
    }

    /// How many prompts are waiting behind it.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The permission requests the agent is blocked on, oldest first.
    pub fn pending_permissions(&self) -> &[RequestId] {
        &self.outstanding
    }

    /// The resolved system context this session was started with. Read by the inspector to
    /// show what the agent was actually told, rather than re-deriving it and being able to
    /// disagree — the `rules.rs` rule applied one level down.
    pub fn system_context(&self) -> &str {
        &self.system_context
    }

    /// Whether the process behind this session is still there.
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    /// Asks the agent something.
    ///
    /// **Queues rather than interleaves** when a turn is in flight; see the module note. The
    /// queue is bounded, and a full one is refused with a message rather than dropping the
    /// prompt silently.
    pub fn prompt(&mut self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        if self.current.is_some() {
            if self.queue.len() >= MAX_QUEUED {
                return Err(AgentError::Refused(format!(
                    "this agent already has {MAX_QUEUED} prompts waiting — let it catch up \
                     before adding more"
                )));
            }
            self.queue.push_back(text);
            return Ok(());
        }
        self.dispatch(text)
    }

    /// Stops the turn in flight. Harmless when there is none.
    ///
    /// **Does not clear the queue**, deliberately: cancelling the answer to one question is
    /// not withdrawing the ones asked after it. [`Session::clear_queue`] is the separate
    /// verb for that, so a user who meant "stop everything" says so.
    pub fn cancel(&mut self) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        self.transport.cancel()
    }

    /// Forgets the prompts that have not started. Answers how many were dropped.
    pub fn clear_queue(&mut self) -> usize {
        let dropped = self.queue.len();
        self.queue.clear();
        dropped
    }

    /// Answers a permission request. Unknown ids are ignored — a stale dialog answered twice
    /// must not be an error.
    pub fn answer_permission(&mut self, id: &RequestId, allowed: bool) -> Result<()> {
        if !self.outstanding.iter().any(|pending| pending == id) {
            return Ok(());
        }
        self.transport.answer_permission(id, allowed)
    }

    /// Types into the agent's terminal. Refused by the transports that have none.
    pub fn write_input(&mut self, text: &str) -> Result<()> {
        self.transport.write_input(text)
    }

    /// Takes the images the agent produced, for the caller to put in the blob store.
    ///
    /// ⚠ **Drain this before [`Session::poll`] and substitute the real hashes before the
    /// events are appended to the sidecar.** The transport parks the bytes before it sends
    /// the event, so blobs-then-events is the order in which the map is always populated
    /// when the `Image` event arrives — and a transcript written with a placeholder still in
    /// it reads back, after a restart, as a picture that resolves to nothing.
    pub fn take_pending_blobs(&mut self) -> Vec<PendingBlob> {
        self.transport.take_pending_blobs()
    }

    /// Everything that has arrived since the last call, in order.
    ///
    /// **Never blocks**, and never returns more than [`POLL_BUDGET`] events. Also where the
    /// queue drains: a turn ending here starts the next prompt, so a queued prompt costs one
    /// frame of latency rather than needing anything to notice it.
    pub fn poll(&mut self) -> Vec<TranscriptEvent> {
        let mut drained = Vec::with_capacity(8);
        for _ in 0..POLL_BUDGET {
            match self.events.try_recv() {
                Ok(event) => {
                    self.absorb(&event);
                    drained.push(event);
                }
                // Disconnected cannot happen while this session lives — it holds a sender of
                // its own — so it is treated as an empty channel rather than as a failure.
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        self.start_next();
        drained
    }

    /// Ends the session and releases the process. Idempotent.
    pub fn shutdown(&mut self) -> Result<()> {
        self.queue.clear();
        self.transport.shutdown()
    }

    /// Applies one event to the state machine.
    fn absorb(&mut self, event: &TranscriptEvent) {
        match event {
            TranscriptEvent::TurnStarted { turn, prompt } => {
                self.current = Some(*turn);
                // A new turn clears a previous failure: the node is working again, and
                // leaving it red would make every later success look broken.
                self.status = Status::Running;
                self.detail = headline(prompt);
            }
            TranscriptEvent::PermissionRequest { id, summary, .. } => {
                if !self.outstanding.iter().any(|pending| pending == id) {
                    self.outstanding.push(id.clone());
                }
                self.status = Status::WaitingForPermission;
                self.detail = headline(summary);
            }
            TranscriptEvent::PermissionAnswer { id, .. } => {
                self.outstanding.retain(|pending| pending != id);
                if self.outstanding.is_empty() && self.current.is_some() {
                    self.status = Status::Running;
                }
            }
            TranscriptEvent::TurnEnded { turn, outcome } => {
                // A `TurnEnded` for a turn that is not the current one is a late arrival
                // from a turn already abandoned; it must not clear the state of the new one.
                if self.current != Some(*turn) {
                    return;
                }
                self.current = None;
                // A question nobody answered before the turn ended cannot be answered now:
                // the agent that asked it has stopped waiting.
                self.outstanding.clear();
                match outcome {
                    TurnOutcome::Completed => {
                        self.status = Status::Idle;
                        self.detail.clear();
                    }
                    TurnOutcome::Cancelled => {
                        self.status = Status::Idle;
                        self.detail = "stopped".into();
                    }
                    TurnOutcome::Failed { message } => {
                        self.status = Status::Error;
                        self.detail = headline(message);
                    }
                    TurnOutcome::Exhausted { message } => {
                        // Not an error: the agent answered as far as it could, and painting
                        // it red would make a long answer look like a broken one.
                        self.status = Status::Idle;
                        self.detail = headline(message);
                    }
                }
            }
            TranscriptEvent::Error { message } => {
                self.status = Status::Error;
                self.detail = headline(message);
            }
            _ => {}
        }
    }

    /// Starts the next queued prompt, if the agent is free to take one.
    fn start_next(&mut self) {
        if self.current.is_some() || self.queue.is_empty() {
            return;
        }
        let Some(next) = self.queue.pop_front() else {
            return;
        };
        if let Err(error) = self.dispatch(next) {
            self.fail(&error);
        }
    }

    /// Allocates the turn id and hands the prompt to the transport.
    fn dispatch(&mut self, text: String) -> Result<()> {
        let turn = TurnId(self.next_turn);
        self.next_turn += 1;
        // Optimistic: the transport emits `TurnStarted`, which is what really sets the
        // state. Recording the turn here as well is what stops a second prompt arriving in
        // the same frame from being dispatched before that event has been drained.
        self.current = Some(turn);
        self.status = Status::Running;
        self.detail = headline(&text);

        match self.transport.send_prompt(turn, &text) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.current = None;
                self.fail(&error);
                Err(error)
            }
        }
    }

    /// Records a session-level failure, in the state **and** in the transcript.
    fn fail(&mut self, error: &AgentError) {
        self.status = Status::Error;
        self.detail = headline(&error.to_string());
        let _ = self.voice.send(TranscriptEvent::Error { message: error.to_string() });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.transport.shutdown();
    }
}

/// A one-line, bounded summary for the status row.
///
/// Bounded by **characters, not bytes** — `text[..90]` panics on any multi-byte character
/// straddling the boundary, and this string comes from arbitrary agent output, which is the
/// least controlled input in the application. `strip_site_affix` aborted the whole process
/// twice on exactly this shape of bug (feedback 30), and `panic = "abort"` means there is no
/// catching it.
fn headline(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(90).collect();
    if out.chars().count() < line.chars().count() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::ToolCallId;
    use std::sync::{Arc, Mutex};

    /// A transport that does nothing but record what it was asked, and lets the test play
    /// the agent's part by hand.
    ///
    /// This is the whole reason [`Session::over`] exists: every rule in the state machine is
    /// then an offline test on a machine with no agent installed, which is what `docs/07`
    /// §13 asks of pure logic.
    #[derive(Clone, Default)]
    struct Fake {
        sent: Arc<Mutex<Vec<(TurnId, String)>>>,
        answered: Arc<Mutex<Vec<(RequestId, bool)>>>,
        cancels: Arc<Mutex<usize>>,
        shutdowns: Arc<Mutex<usize>>,
        /// When set, `send_prompt` fails — a missing binary, a closed pipe.
        broken: Arc<Mutex<bool>>,
    }

    impl AgentTransport for Fake {
        fn kind(&self) -> TransportKind {
            TransportKind::Acp
        }

        fn send_prompt(&mut self, turn: TurnId, prompt: &str) -> Result<()> {
            if *self.broken.lock().unwrap() {
                return Err(AgentError::Transport {
                    transport: "acp",
                    message: "the pipe is closed".into(),
                });
            }
            self.sent.lock().unwrap().push((turn, prompt.to_owned()));
            Ok(())
        }

        fn cancel(&mut self) -> Result<()> {
            *self.cancels.lock().unwrap() += 1;
            Ok(())
        }

        fn answer_permission(&mut self, id: &RequestId, allowed: bool) -> Result<()> {
            self.answered.lock().unwrap().push((id.clone(), allowed));
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            *self.shutdowns.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// The session, the fake behind it, and the sender the test speaks through as if it were
    /// the agent.
    fn session() -> (Session, Fake, Sender<TranscriptEvent>) {
        let fake = Fake::default();
        let (voice, events) = channel();
        let session =
            Session::over(Box::new(fake.clone()), voice.clone(), events, "you are a tester".into());
        (session, fake, voice)
    }

    fn started(turn: u64, prompt: &str) -> TranscriptEvent {
        TranscriptEvent::TurnStarted { turn: TurnId(turn), prompt: prompt.into() }
    }

    fn ended(turn: u64, outcome: TurnOutcome) -> TranscriptEvent {
        TranscriptEvent::TurnEnded { turn: TurnId(turn), outcome }
    }

    /// The ordinary life of a turn, and the two things the painter reads from it.
    #[test]
    fn a_turn_runs_and_the_session_goes_back_to_idle() {
        let (mut session, fake, agent) = session();
        assert_eq!(session.status(), Status::Idle);
        assert_eq!(session.system_context(), "you are a tester");

        session.prompt("what is 2 + 2").unwrap();
        assert_eq!(fake.sent.lock().unwrap().as_slice(), [(TurnId(1), "what is 2 + 2".into())]);
        assert_eq!(session.status(), Status::Running, "the state did not wait for an event");

        agent.send(started(1, "what is 2 + 2")).unwrap();
        agent.send(TranscriptEvent::Text { text: "4".into() }).unwrap();
        agent.send(ended(1, TurnOutcome::Completed)).unwrap();

        let drained = session.poll();
        assert_eq!(drained.len(), 3, "poll did not hand back everything that had arrived");
        assert_eq!(session.status(), Status::Idle);
        assert_eq!(session.current_turn(), None);
        assert!(session.detail().is_empty());
    }

    /// **The queue rule.** Two prompts in one turn is not a thing any transport can express;
    /// the assertion that matters is that the second reaches the transport *only after* the
    /// first turn ended, and that it does so without anything else prodding the session.
    #[test]
    fn a_prompt_that_arrives_mid_turn_waits_for_the_one_in_flight() {
        let (mut session, fake, agent) = session();
        session.prompt("first").unwrap();
        session.prompt("second").unwrap();
        session.prompt("third").unwrap();

        assert_eq!(session.queued(), 2);
        assert_eq!(fake.sent.lock().unwrap().len(), 1, "a queued prompt was sent anyway");

        agent.send(started(1, "first")).unwrap();
        agent.send(ended(1, TurnOutcome::Completed)).unwrap();
        session.poll();

        // The turn ended, so the next prompt went out — in the same poll, with its own id.
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1], (TurnId(2), "second".into()));
        assert_eq!(session.queued(), 1);
        assert_eq!(session.status(), Status::Running);

        agent.send(started(2, "second")).unwrap();
        agent.send(ended(2, TurnOutcome::Completed)).unwrap();
        session.poll();
        assert_eq!(fake.sent.lock().unwrap()[2], (TurnId(3), "third".into()));

        // Turn ids are monotonic and never reused: they are the key a transcript on disk is
        // read back by, so two turns sharing one would make the file unreplayable.
        let ids: Vec<u64> = fake.sent.lock().unwrap().iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// The queue is bounded and says so. An orchestrator in a loop can queue faster than a
    /// worker answers, and a board that silently swallowed the excess would work through
    /// hours of stale instructions with nothing on screen to explain why.
    #[test]
    fn the_queue_is_bounded_and_refuses_rather_than_dropping() {
        let (mut session, _fake, _agent) = session();
        session.prompt("in flight").unwrap();
        for index in 0..MAX_QUEUED {
            session.prompt(format!("queued {index}")).unwrap();
        }
        let refused = session.prompt("one too many").unwrap_err();
        assert!(matches!(refused, AgentError::Refused(_)), "{refused}");
        assert_eq!(session.queued(), MAX_QUEUED);

        // Cancelling the turn does not withdraw the questions asked after it; clearing does.
        assert_eq!(session.clear_queue(), MAX_QUEUED);
        assert_eq!(session.queued(), 0);
    }

    /// A blocked agent is **not** the same as a working one: the thing that unblocks it is a
    /// click, and the board draws attention to it for that reason.
    #[test]
    fn a_permission_request_blocks_the_session_until_it_is_answered() {
        let (mut session, fake, agent) = session();
        session.prompt("edit the file").unwrap();
        agent.send(started(1, "edit the file")).unwrap();

        let asking = RequestId("p1".into());
        agent
            .send(TranscriptEvent::PermissionRequest {
                id: asking.clone(),
                summary: "write to src/main.rs".into(),
                detail: "…".into(),
            })
            .unwrap();
        session.poll();

        assert_eq!(session.status(), Status::WaitingForPermission);
        assert!(session.status().needs_attention(), "a blocked agent must ask for a person");
        assert!(session.status().is_busy(), "the turn has not ended");
        assert_eq!(session.detail(), "write to src/main.rs");
        assert_eq!(session.pending_permissions(), std::slice::from_ref(&asking));

        session.answer_permission(&asking, true).unwrap();
        assert_eq!(*fake.answered.lock().unwrap(), [(asking.clone(), true)]);

        // The transport is what emits the answer, so the state follows the stream rather
        // than the call — which is what keeps the sidecar's record and the node in step.
        agent.send(TranscriptEvent::PermissionAnswer { id: asking, allowed: true }).unwrap();
        session.poll();
        assert_eq!(session.status(), Status::Running);
        assert!(session.pending_permissions().is_empty());

        // An id nobody is waiting on is ignored rather than an error: a stale dialog
        // answered twice must not fail.
        session.answer_permission(&RequestId("gone".into()), true).unwrap();
        assert_eq!(fake.answered.lock().unwrap().len(), 1);
    }

    /// A question the agent stopped waiting for cannot be answered. Without this, a node
    /// whose turn failed mid-question sits on "Needs you" forever and the only way out is a
    /// restart.
    #[test]
    fn a_turn_ending_clears_a_question_nobody_answered() {
        let (mut session, _fake, agent) = session();
        session.prompt("go").unwrap();
        agent.send(started(1, "go")).unwrap();
        agent
            .send(TranscriptEvent::PermissionRequest {
                id: RequestId("p1".into()),
                summary: "run rm -rf".into(),
                detail: String::new(),
            })
            .unwrap();
        agent.send(ended(1, TurnOutcome::Cancelled)).unwrap();
        session.poll();

        assert!(session.pending_permissions().is_empty());
        assert_eq!(session.status(), Status::Idle);
        assert_eq!(session.detail(), "stopped");
    }

    /// The four outcomes reach three states, and **exhausted is not an error**: the agent
    /// answered as far as it could, and painting it red would make a long answer look broken.
    #[test]
    fn the_outcomes_reach_the_states_the_painter_draws() {
        for (outcome, expected) in [
            (TurnOutcome::Completed, Status::Idle),
            (TurnOutcome::Cancelled, Status::Idle),
            (TurnOutcome::Exhausted { message: "hit the context limit".into() }, Status::Idle),
            (TurnOutcome::Failed { message: "the pipe closed".into() }, Status::Error),
        ] {
            let (mut session, _fake, agent) = session();
            session.prompt("go").unwrap();
            agent.send(started(1, "go")).unwrap();
            agent.send(ended(1, outcome.clone())).unwrap();
            session.poll();
            assert_eq!(session.status(), expected, "{outcome:?}");
        }

        // And a later turn clears the failure — leaving the node red through a working turn
        // would make every later success look broken.
        let (mut session, _fake, agent) = session();
        session.prompt("go").unwrap();
        agent.send(started(1, "go")).unwrap();
        agent.send(ended(1, TurnOutcome::Failed { message: "boom".into() })).unwrap();
        session.poll();
        assert_eq!(session.status(), Status::Error);

        session.prompt("try again").unwrap();
        agent.send(started(2, "try again")).unwrap();
        session.poll();
        assert_eq!(session.status(), Status::Running);
    }

    /// A prompt that cannot be sent must not leave the session stuck in `Running` with
    /// nothing coming — and the failure belongs in the **transcript**, not only in a status
    /// line, or the sidecar shows a question the agent simply never answered.
    ///
    /// ⚠ **The `TurnStarted` assertion below is the transport contract, not a detail.**
    /// [`crate::transport::AgentTransport::send_prompt`] promises that an `Err` means no
    /// `TurnStarted` was emitted, and [`Session::absorb`] is why: that event sets `current`
    /// and `Status::Running` unconditionally, and nothing but a matching `TurnEnded` clears
    /// them. A transport that emitted the start and *then* failed — which both the CLI and
    /// the ACP transport did, with `.spawn(…)?` sitting between the two — leaves a session
    /// that refuses every later prompt with *"this agent is still working"* for the life of
    /// the process. `dispatch`'s own `Err` arm cannot undo it, because the event is already
    /// in the channel by then.
    #[test]
    fn a_prompt_that_could_not_be_sent_fails_the_session_and_says_so_in_the_stream() {
        let (mut session, fake, _agent) = session();
        *fake.broken.lock().unwrap() = true;

        let error = session.prompt("go").unwrap_err();
        assert!(error.to_string().contains("pipe"), "{error}");
        assert_eq!(session.status(), Status::Error);
        assert_eq!(session.current_turn(), None, "a failed send left a turn in flight");

        let drained = session.poll();
        assert!(
            !drained
                .iter()
                .any(|event| matches!(event, TranscriptEvent::TurnStarted { .. })),
            "a refused prompt announced a turn that nothing will ever end: {drained:?}"
        );
        assert!(
            matches!(drained.as_slice(), [TranscriptEvent::Error { .. }]),
            "the failure never reached the transcript: {drained:?}"
        );

        // The session is still usable once the transport recovers.
        *fake.broken.lock().unwrap() = false;
        session.prompt("again").unwrap();
        assert_eq!(session.status(), Status::Running);
    }

    /// Bounded per frame. A build log arriving faster than the display refreshes must not
    /// turn one frame into an unbounded amount of work — and what is left over must still be
    /// there, or the transcript would silently lose the middle of every long run.
    #[test]
    fn a_poll_is_bounded_and_keeps_the_rest_for_the_next_frame() {
        let (mut session, _fake, agent) = session();
        for index in 0..POLL_BUDGET + 25 {
            agent.send(TranscriptEvent::Terminal { text: format!("line {index}") }).unwrap();
        }

        let first = session.poll();
        assert_eq!(first.len(), POLL_BUDGET, "the frame budget was not honoured");
        let second = session.poll();
        assert_eq!(second.len(), 25, "the remainder was lost");
        assert!(session.poll().is_empty());
    }

    /// Cancelling with nothing in flight must not reach the transport: an ACP `session/cancel`
    /// for no turn is a protocol error on some agents, and a `Ctrl-C` typed into an idle
    /// terminal interrupts whatever the user was doing there.
    #[test]
    fn cancelling_an_idle_session_touches_nothing() {
        let (mut session, fake, agent) = session();
        session.cancel().unwrap();
        assert_eq!(*fake.cancels.lock().unwrap(), 0);

        session.prompt("go").unwrap();
        agent.send(started(1, "go")).unwrap();
        session.poll();
        session.cancel().unwrap();
        assert_eq!(*fake.cancels.lock().unwrap(), 1);
    }

    /// A late event from a turn that is over must not touch the state of the one after it.
    /// Reachable in practice: a cancel and a fresh prompt in the same frame.
    #[test]
    fn a_late_end_from_an_abandoned_turn_is_ignored() {
        let (mut session, _fake, agent) = session();
        session.prompt("first").unwrap();
        agent.send(started(1, "first")).unwrap();
        agent.send(ended(1, TurnOutcome::Cancelled)).unwrap();
        session.poll();

        session.prompt("second").unwrap();
        agent.send(started(2, "second")).unwrap();
        // The abandoned turn's end, arriving after the new one started.
        agent.send(ended(1, TurnOutcome::Failed { message: "too late".into() })).unwrap();
        session.poll();

        assert_eq!(session.status(), Status::Running, "a stale end stopped the live turn");
        assert_eq!(session.current_turn(), Some(TurnId(2)));
    }

    /// Everything that is not part of the state machine passes through untouched — the
    /// session is a state machine over the stream, not a filter on it.
    #[test]
    fn output_events_pass_through_without_changing_the_state() {
        let (mut session, _fake, agent) = session();
        session.prompt("go").unwrap();
        agent.send(started(1, "go")).unwrap();
        session.poll();

        for event in [
            TranscriptEvent::Text { text: "prose".into() },
            TranscriptEvent::Thought { text: "reasoning".into() },
            TranscriptEvent::ToolCall {
                id: ToolCallId("t".into()),
                name: "bash".into(),
                input: "ls".into(),
            },
            TranscriptEvent::Image { blob: "pending:0".into(), caption: None },
        ] {
            agent.send(event).unwrap();
        }
        let drained = session.poll();
        assert_eq!(drained.len(), 4);
        assert_eq!(session.status(), Status::Running);
        assert_eq!(session.detail(), "go", "output overwrote what the turn is about");
    }

    /// A status line is built from arbitrary agent output, so it must not be able to panic —
    /// the `strip_site_affix` family of bug, which aborted the whole process twice.
    #[test]
    fn a_detail_line_never_panics_on_multibyte_or_empty_text() {
        for text in ["", "   ", "\n\n", &"夕".repeat(400), &"🙂".repeat(200), "one\ntwo"] {
            let line = headline(text);
            assert!(line.chars().count() <= 91, "{line}");
        }
        assert_eq!(headline("first line\nsecond"), "first line");
        assert!(headline(&"x".repeat(200)).ends_with('…'));
    }

    #[test]
    fn shutting_down_releases_the_transport_and_is_idempotent() {
        let (mut session, fake, _agent) = session();
        session.prompt("go").unwrap();
        session.shutdown().unwrap();
        session.shutdown().unwrap();
        assert_eq!(*fake.shutdowns.lock().unwrap(), 2, "shutdown must reach the transport");
        assert_eq!(session.queued(), 0);
        drop(session);
        // Dropping calls it again — the transport's own `shutdown` is what must be
        // idempotent, and every one of them is.
        assert_eq!(*fake.shutdowns.lock().unwrap(), 3);
    }
}
