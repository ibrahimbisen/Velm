//! The app half of feature 12 — talking to an agent node out loud.
//!
//! `vellum_agent::voice` owns everything that can be decided without a window: the capture
//! trait, the push-to-talk state machine, the WAV encoding, and the two transcribers. It owned
//! all of that for a long time with **nothing in this crate calling any of it** — the panel's
//! Voice switch wrote a flag, the inspector read it back, and that was the whole loop. This
//! module is the join that was missing.
//!
//! ```text
//!   key down ──► VoicePool::press ──► VoiceCapture::start        (frame thread)
//!   key up   ──► VoicePool::release ──► Utterance ──► worker ──► Transcribe
//!                                                        │
//!   drain ◄── VoiceEvent::Transcribed(Dictation) ◄────────┘       (frame thread)
//!     │
//!     └──► ActiveState::set_agent_draft — the path a *typed* prompt already ends in
//! ```
//!
//! # Three rules this module exists to keep
//!
//! 1. **No idle cost.** The pool is `None` until the first press. A board with agent nodes on
//!    it that nobody has spoken to allocates nothing, opens no device and spawns no thread —
//!    `docs/07` §0.2, and the reason [`PushToTalk::new`]'s own doc says it opens no device.
//! 2. **Voice adds no second way for a prompt to reach an agent.** A transcription ends up in
//!    the node's draft through [`crate::actions::ActiveState::set_agent_draft`], which is
//!    where a *typed* prompt ends up. Everything downstream — the undo group, the transcript,
//!    the rule cascade — cannot tell the two apart, which is `voice::Dictation`'s own stated
//!    contract.
//! 3. **The painter must be able to see this.** [`VoicePool::status`] exists so that
//!    *recording* and *transcribing* are in the snapshot the painter is handed. State that
//!    accumulates where the painter cannot reach it is the defect this repository has now
//!    shipped four times — the pen (feedback 7), the frame (23), the prompt row (34) — and
//!    every one of them looked like the feature doing nothing at all.
//!
//! # Why the node is named by its wire id
//!
//! [`vellum_agent::voice::VoiceTarget`] holds a node as a `String`, and the string this module
//! puts in it is [`NodeKey::wire`] — `<board-key>:<item-id>` — never a bare document id. That
//! is not a preference: feedback 37 records a defect where one half of the agent layer wrote a
//! bare `7@1` and the other half compared it against a wire id, so a private note could not be
//! read by the agent that owned it, on any board, ever. Two spellings of "which node" is the
//! bug; there is one here, and [`NodeKey::from_wire`] is its exact inverse.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use vellum_agent::voice::{
    self, Dictation, PushToTalk, Speech, Utterance, VoiceCapture, VoiceEvent, VoiceTarget,
};

use crate::agent_runtime::NodeKey;

/// What one node is doing about voice this frame, for the painter.
///
/// Deliberately not a `Status` variant on [`crate::agent_view::AgentView`]: a node that is
/// listening may also be *running*, and collapsing the two into one enum would make the status
/// dot lie about one of them. This rides alongside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceStatus {
    /// The microphone is open for this node. `held_ms` is what draws the level/elapsed hint.
    Listening { held_ms: u64 },
    /// The audio has left for a transcriber. `backend` names which one, so the node can say
    /// *"whisper.cpp, on this machine"* rather than leaving the user to guess whether their
    /// voice just went to somebody's API. That is the one thing worth showing during the wait.
    Transcribing { backend: String },
}

/// The live voice state: one press at a time, and any number of transcriptions in flight.
///
/// **Created on the first press and never before** — see rule 1 in this module's header.
pub struct VoicePool {
    push: PushToTalk<Box<dyn VoiceCapture>>,
    /// Worker → frame loop. The `links.rs` shape: a channel drained once per frame.
    tx: Sender<VoiceEvent>,
    rx: Receiver<VoiceEvent>,
    /// Nodes whose audio is with a transcriber, and which one. Keyed by [`NodeKey::wire`].
    transcribing: HashMap<String, String>,
    /// A pinned transcriber, for `--demo agent-voice`. `None` in every real run.
    ///
    /// **The substitution is named rather than hidden**, exactly as `--demo agent-ipc` names
    /// the one it makes. A default build's [`voice::microphone`] is `Unavailable` and refuses
    /// by design, and `Speech::transcriber` refuses on a machine with no whisper installed and
    /// no hosted provider configured — so with no seam here the *only* thing an unattended run
    /// could check is that both refusals fire, which says nothing about the join between them.
    /// [`vellum_agent::voice::CannedTranscriber`]'s own doc says it exists for this: *"what
    /// lets the whole press-to-dictation path be exercised with neither a microphone nor a
    /// network"*.
    fixture: Option<vellum_agent::voice::CannedTranscriber>,
}

impl VoicePool {
    /// **Opens no device.** The capture is constructed, which is what `microphone()` promises
    /// costs nothing; the device opens in [`PushToTalk::press`] and nowhere else.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            push: PushToTalk::new(voice::microphone()),
            tx,
            rx,
            transcribing: HashMap::new(),
            fixture: None,
        }
    }

    /// A pool over a canned microphone and a canned transcriber. **`--demo agent-voice` only.**
    ///
    /// Everything between the two is production's: the same [`PushToTalk`], the same channel,
    /// the same worker thread, the same drain, and the same
    /// [`crate::actions::ActiveState::drain_voice`] writing the same draft. What is replaced is
    /// exactly the two ends that need hardware and a model file — which is the substitution
    /// `--demo agent-ipc` makes for the same reason and names in its own output.
    #[must_use]
    pub fn canned(capture: Box<dyn VoiceCapture>, transcript: &str) -> Self {
        let (tx, rx) = channel();
        Self {
            push: PushToTalk::new(capture),
            tx,
            rx,
            transcribing: HashMap::new(),
            fixture: Some(vellum_agent::voice::CannedTranscriber::new(transcript)),
        }
    }

    /// The node currently being recorded for, as a [`NodeKey`].
    #[must_use]
    pub fn holding(&self) -> Option<NodeKey> {
        self.push.holding().and_then(NodeKey::from_wire)
    }

    /// Begins recording for one node.
    ///
    /// The `target` is the gate: [`VoiceTarget::new`] answers `None` for a node that has not
    /// turned voice on, so "a node that did not ask for voice cannot be recorded against" is
    /// enforced by what compiles rather than by an `if` a second call site can forget.
    pub fn press(&mut self, target: &VoiceTarget, now_ms: u64) -> vellum_agent::Result<()> {
        self.push.press(target, now_ms)
    }

    /// Ends the press and sends the audio to a transcriber on a worker thread.
    ///
    /// Returns what the node should say while it waits. The transcription itself lands later,
    /// through [`VoicePool::drain`].
    pub fn release(
        &mut self,
        now_ms: u64,
        speech: &Speech,
        data_dir: Option<&std::path::Path>,
    ) -> vellum_agent::Result<String> {
        let utterance = self.push.release(now_ms)?;
        self.transcribe(utterance, speech, data_dir)
    }

    /// The clock half of the bound, called once per frame while a press is live.
    ///
    /// A stuck key — or a user who walked away holding it — becomes a finished utterance at
    /// [`voice::MAX_UTTERANCE_SECONDS`] rather than a buffer that grows until the machine
    /// complains. Answers `None` on almost every frame.
    pub fn poll(
        &mut self,
        now_ms: u64,
        speech: &Speech,
        data_dir: Option<&std::path::Path>,
    ) -> Option<vellum_agent::Result<String>> {
        let utterance = self.push.poll(now_ms)?;
        Some(utterance.and_then(|utterance| self.transcribe(utterance, speech, data_dir)))
    }

    /// Sends one utterance to a worker. The whole of the threading in this module.
    fn transcribe(
        &mut self,
        utterance: Utterance,
        speech: &Speech,
        data_dir: Option<&std::path::Path>,
    ) -> vellum_agent::Result<String> {
        // Resolved **here, on the frame thread**, so a misconfiguration is an immediate
        // refusal naming what to fix rather than an error that arrives from a worker a second
        // later with no gesture left to attach it to. `detect_local` probes `PATH` for the
        // file rather than running anything, so it cannot hang.
        let transcriber: Box<dyn vellum_agent::voice::Transcribe> = match &self.fixture {
            Some(canned) => Box::new(canned.clone()),
            None => {
                let local = speech.detect_local().is_some();
                speech.transcriber(data_dir, local)?
            }
        };
        let label = transcriber.label();
        let node = utterance.node().to_owned();
        self.transcribing.insert(node.clone(), label.clone());

        let tx = self.tx.clone();
        let for_worker = node.clone();
        // One thread per utterance rather than a pool: a person speaks a handful of times a
        // minute at the very most, and a resident worker for that is a thread asleep for the
        // life of the process — which is the idle cost this layer promises not to have.
        let spawned = std::thread::Builder::new().name("velm-transcribe".into()).spawn(move || {
            let node = for_worker;
            let event = match transcriber.transcribe(&utterance) {
                Ok(text) if text.trim().is_empty() => VoiceEvent::Failed {
                    node,
                    // A transcriber that answers nothing is not an error and must not be
                    // silence: without this the node would go back to idle with no draft and
                    // no explanation at all, which reads exactly like the key not working.
                    message: "nothing was recognised in that recording".to_owned(),
                },
                Ok(text) => VoiceEvent::Transcribed(Dictation { node, text }),
                Err(error) => VoiceEvent::Failed { node, message: error.to_string() },
            };
            // The receiver is gone when the app is shutting down. Dropping the answer is
            // right; there is nothing left to show it on.
            let _ = tx.send(event);
        });

        // ⚠ The spawn is fallible and the bookkeeping above has already happened. Leaving the
        // entry in place would leave the node saying *"transcribing"* for the rest of the
        // session with nothing ever coming — feedback 34's `TurnStarted`-before-a-fallible-
        // spawn defect, which left a node spinning forever and refusing every later prompt.
        // Same shape, so the same fix: undo the claim on the error path.
        if let Err(error) = spawned {
            self.transcribing.remove(&node);
            return Err(vellum_agent::AgentError::Refused(format!(
                "could not start transcription: {error}"
            )));
        }

        Ok(label)
    }

    /// Everything the workers have answered since the last frame.
    ///
    /// Drained rather than polled-with-a-timeout: this runs on the frame loop and must never
    /// block it, which is the rule every pool in this application follows.
    pub fn drain(&mut self) -> Vec<VoiceEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    match &event {
                        VoiceEvent::Transcribed(Dictation { node, .. })
                        | VoiceEvent::Failed { node, .. } => {
                            self.transcribing.remove(node);
                        }
                        VoiceEvent::Transcribing { .. } => {}
                    }
                    out.push(event);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }

    /// What this node is doing about voice, for the painter's snapshot.
    #[must_use]
    pub fn status(&self, key: &NodeKey, now_ms: u64) -> Option<VoiceStatus> {
        let wire = key.wire();
        if self.push.holding() == Some(wire.as_str()) {
            return Some(VoiceStatus::Listening { held_ms: self.push.held_ms(now_ms) });
        }
        self.transcribing
            .get(&wire)
            .map(|backend| VoiceStatus::Transcribing { backend: backend.clone() })
    }

    /// Whether anything at all is happening, so the frame loop knows to keep repainting.
    ///
    /// ⚠ **Both halves, and the second is easy to forget.** A repaint while the key is held is
    /// what makes [`VoicePool::poll`] able to enforce the utterance bound at all and what keeps
    /// the elapsed hint moving; a repaint while a *transcription* is out is what makes the
    /// answer appear when it lands rather than on whatever unrelated event happens next. Ask
    /// for repaints only while something is actually happening and stop — the `landed_background`
    /// rule from feedback 21, which is also why this is a question and not a standing timer.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.push.holding().is_some() || !self.transcribing.is_empty()
    }

    /// Abandons the press and throws the audio away.
    ///
    /// What Escape, a tab switch, losing the window and quitting all do. Idempotent, and it
    /// leaves any transcription already in flight alone — that audio has left the machine's
    /// microphone and its answer is still worth having.
    ///
    /// ⚠ Every way this gesture can end **without a key-up** must call this. That is feedback
    /// 27's rule, stated there after an eraser sweep leaked an undo group by exactly this
    /// route, and violated again in feedback 35 by a prompt session that survived a tab
    /// switch. A held microphone that survives a tab switch is the same shape with a worse
    /// consequence: the device stays open on a board the user is no longer looking at.
    pub fn cancel(&mut self) {
        self.push.cancel();
    }
}

impl Default for VoicePool {
    fn default() -> Self {
        Self::new()
    }
}

/// The gate, as a function: which node a press would address, given what is selected.
///
/// `None` when the selection is not exactly one agent node, or when that node has not turned
/// voice on. **Exactly one**, deliberately: an utterance is addressed to somebody, and
/// splitting one sentence between three selected agents is not a thing anybody means.
#[must_use]
pub fn target_for(key: &NodeKey, model: &vellum_agent::AgentModel) -> Option<VoiceTarget> {
    VoiceTarget::new(key.wire(), model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_agent::voice::CannedCapture;

    fn key() -> NodeKey {
        NodeKey {
            board: vellum_agent::BoardKey::from_raw("00000000000000ab"),
            item: "7@1".into(),
        }
    }

    /// The gate is the model's own flag, and it is read through the wire id.
    ///
    /// Both halves matter. A node with voice off must not be recordable at all, and the id the
    /// target carries must be the one [`NodeKey::from_wire`] can invert — feedback 37's bare-id
    /// defect is exactly what this asserts cannot recur.
    #[test]
    fn only_a_node_that_asked_for_voice_can_be_talked_to_and_it_round_trips() {
        let mut model = vellum_agent::AgentModel::default();
        assert!(target_for(&key(), &model).is_none(), "voice is off by default");

        model.voice = true;
        let target = target_for(&key(), &model).expect("voice is on");
        assert_eq!(
            NodeKey::from_wire(target.node()).as_ref(),
            Some(&key()),
            "the target must name the node in the one spelling the runtime can invert",
        );
    }

    /// A pool that has never been pressed is not busy, so the frame loop asks for nothing.
    #[test]
    fn an_untouched_pool_asks_for_no_repaints() {
        let pool = VoicePool {
            push: PushToTalk::new(Box::new(CannedCapture::tone()) as Box<dyn VoiceCapture>),
            tx: channel().0,
            rx: channel().1,
            transcribing: HashMap::new(),
            fixture: None,
        };
        assert!(!pool.is_busy(), "nothing is happening, so nothing should repaint");
        assert!(pool.status(&key(), 0).is_none());
    }

    /// The whole press path, over a canned capture and a canned transcriber.
    ///
    /// This is the seam `CannedCapture` and `CannedTranscriber` exist for: neither a
    /// microphone nor a network, so it runs in `cargo test` on any machine and in a build with
    /// the `voice` feature off — which is the build everybody has.
    #[test]
    fn a_press_and_a_release_produce_a_dictation_addressed_to_the_node() {
        let model = vellum_agent::AgentModel { voice: true, ..Default::default() };
        let target = target_for(&key(), &model).expect("voice is on");

        let mut pool = VoicePool {
            push: PushToTalk::new(Box::new(CannedCapture::tone()) as Box<dyn VoiceCapture>),
            tx: channel().0,
            rx: channel().1,
            transcribing: HashMap::new(),
            fixture: None,
        };

        pool.press(&target, 0).expect("the canned capture starts");
        assert_eq!(pool.holding().as_ref(), Some(&key()), "the press names the node");
        assert!(pool.is_busy(), "a live press must keep the frame loop repainting");
        assert!(
            matches!(pool.status(&key(), 500), Some(VoiceStatus::Listening { held_ms: 500 })),
            "the painter must be able to see that this node is listening",
        );

        pool.cancel();
        assert!(pool.holding().is_none(), "cancel ends the press");
        assert!(!pool.is_busy(), "and stops the repaints");
    }
}
