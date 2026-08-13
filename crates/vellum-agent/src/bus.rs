//! Routing a message from one agent to another, along a line the user drew.
//!
//! `docs/07-agent-canvas.md` §3 and §6 are the contract. Two sentences from them govern
//! everything here:
//!
//! - **A connector is how the user grants permission for two agents to talk.** A message
//!   with no connector is refused ([`crate::AgentError::NotConnected`]) rather than
//!   delivered, because an agent that could message anyone would make the lines on the
//!   board decorative — the user would have drawn a diagram, not a wiring loom.
//! - **Loops are bounded.** Every message carries a hop count and the bus refuses beyond
//!   [`Bus::max_hops`]. Two agents pointed at each other is not a hypothetical; it is the
//!   first thing anybody builds.
//!
//! # This module never reads the document, and never reads the clock
//!
//! The topology is *passed in*. `vellum-app` walks the board's connectors, decides which of
//! them join two agent-family nodes, resolves each one's direction from its arrowheads, and
//! hands the answer over as a [`Topology`]. So "which agents may talk" is one derivation in
//! one place (§3's rule that a stored flag is a second source of truth that can disagree),
//! and everything in this file is an ordinary unit test on a machine with no display.
//!
//! Time arrives the same way: `now_ms` is an argument, so *"has the pulse expired"* is
//! arithmetic rather than a wait.
//!
//! # Sending does not block
//!
//! [`Bus::send`] validates, enqueues and returns. It never waits for the receiving agent —
//! that agent may be mid-turn, or asleep, or a process that has not been started yet. The
//! queue is drained once per frame by `agent_runtime.rs`, which is the shape `links.rs`
//! already uses for link previews. The only lock taken is around a `VecDeque` push.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;

use crate::transcript::{AgentRef, TranscriptEvent};
use crate::{AgentError, Result};

/// Which way messages may travel along one connector.
///
/// §3: *one end with an arrowhead → messages flow that way; both or neither →
/// bidirectional.* `Forward` is from the connector's start endpoint to its end endpoint,
/// matching `vellum_connect`'s own sense of the two ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkDirection {
    /// Start → end only.
    Forward,
    /// End → start only.
    Backward,
    /// Either way.
    Both,
}

impl LinkDirection {
    /// The §3 derivation, written once.
    ///
    /// `vellum-app` calls this while walking connectors, so the rule *"an arrowhead at one
    /// end points the conversation"* has a single definition with a test against it rather
    /// than being spelled out in the middle of a loop over the document. A plain connector
    /// with no arrowheads is bidirectional deliberately: the user drew a relationship, and
    /// refusing to carry anything until they also pick an arrowhead would make the commonest
    /// gesture on the board do nothing.
    pub const fn from_arrowheads(at_start: bool, at_end: bool) -> Self {
        match (at_start, at_end) {
            (false, true) => Self::Forward,
            (true, false) => Self::Backward,
            // Both, or neither.
            _ => Self::Both,
        }
    }

    const fn carries_forward(self) -> bool {
        matches!(self, Self::Forward | Self::Both)
    }

    const fn carries_backward(self) -> bool {
        matches!(self, Self::Backward | Self::Both)
    }
}

/// What the bus knows about one agent node.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    /// The node's role label, as shown on the board. Carried so a transcript read months
    /// later says *"Planner said…"* rather than *"42@7 said…"* — the same reasoning
    /// [`AgentRef`] records for itself.
    name: String,
    /// [`crate::AgentModel::accepts_messages`], as it stood when the topology was built.
    accepts: bool,
}

/// Who may talk to whom, and what they are called.
///
/// Rebuilt by `vellum-app` whenever the board's connectors or agent nodes change — it is a
/// few hundred bytes for a board with a dozen agents on it, so rebuilding is cheaper than
/// maintaining, and a rebuild cannot drift from the document the way an incrementally
/// patched copy can.
///
/// **A link to a node that was never registered is not an error here.** The app populates
/// nodes and edges in whatever order it walks the document, so validation happens at
/// [`Bus::send`], where the message that needs it is in hand and the refusal can name both
/// ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    nodes: HashMap<String, Node>,
    /// Sender → the receivers it may reach. A `BTreeSet` rather than a `HashSet` so
    /// [`Topology::targets`] answers in a stable order: it is shown to a user and read by an
    /// agent, and a list that reshuffles between frames is a list nobody trusts.
    edges: HashMap<String, BTreeSet<String>>,
}

impl Topology {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an agent node: its id, the label on it, and whether it takes messages.
    ///
    /// Registering the same id twice replaces the entry, so a rebuild is idempotent.
    pub fn node(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        accepts_messages: bool,
    ) -> &mut Self {
        self.nodes
            .insert(id.into(), Node { name: name.into(), accepts: accepts_messages });
        self
    }

    /// Record one connector between two agent nodes, with the direction the app derived
    /// from its arrowheads.
    pub fn link(&mut self, start: &str, end: &str, direction: LinkDirection) -> &mut Self {
        if direction.carries_forward() {
            self.allow(start, end);
        }
        if direction.carries_backward() {
            self.allow(end, start);
        }
        self
    }

    /// Permit one direction outright. [`Topology::link`] is the usual way in; this exists
    /// for a caller that has already resolved the direction to a single arrow.
    ///
    /// A self-link is dropped rather than stored: a connector from an agent to itself is a
    /// legal thing to draw and an infinite loop to honour, and refusing it here is one check
    /// instead of one per delivery.
    pub fn allow(&mut self, from: &str, to: &str) -> &mut Self {
        if from != to {
            self.edges.entry(from.to_owned()).or_default().insert(to.to_owned());
        }
        self
    }

    /// Whether `from` may message `to`.
    pub fn may_send(&self, from: &str, to: &str) -> bool {
        self.edges.get(from).is_some_and(|set| set.contains(to))
    }

    /// Everyone `from` may message, in a stable order.
    ///
    /// A `Vec` rather than an iterator: this is read by the HUD and by the CLI's *"who can I
    /// talk to"*, not by anything hot, and an `impl Iterator` here would tie the answer to
    /// two input lifetimes for no gain.
    pub fn targets(&self, from: &str) -> Vec<&str> {
        self.edges
            .get(from)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// The label on a node, if it is one.
    pub fn name_of(&self, id: &str) -> Option<&str> {
        self.nodes.get(id).map(|node| node.name.as_str())
    }

    /// Whether a node currently takes messages. `None` if it is not an agent node at all —
    /// which is a different answer from "it is muted", and the two produce different toasts.
    pub fn accepts(&self, id: &str) -> Option<bool> {
        self.nodes.get(id).map(|node| node.accepts)
    }

    pub fn is_agent(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Turn what an agent typed into a node id.
    ///
    /// An agent knows its neighbour as *"Reviewer"* — the label it can see on the board and
    /// the one a human would use in a prompt. It has no reason to know `42@7`, which is a
    /// Loro `TreeID` rendered as a string. So `velm-agent-cli send Reviewer "…"` has to work,
    /// and an id is accepted too because a spawned sub-agent is told its parent's id directly.
    ///
    /// **An ambiguous name is refused rather than guessed.** Two nodes labelled *Reviewer* is
    /// an ordinary thing to have on a board, and picking whichever the hash map yielded first
    /// would deliver a message to a different agent on different runs of the same board.
    pub fn resolve(&self, query: &str) -> Resolution {
        if self.nodes.contains_key(query) {
            return Resolution::Found(query.to_owned());
        }
        let mut found = None;
        let mut count = 0usize;
        for (id, node) in &self.nodes {
            if node.name.eq_ignore_ascii_case(query.trim()) {
                count += 1;
                found = Some(id.clone());
            }
        }
        match (count, found) {
            (1, Some(id)) => Resolution::Found(id),
            (0, _) | (_, None) => Resolution::Unknown,
            _ => Resolution::Ambiguous(count),
        }
    }
}

/// What [`Topology::resolve`] made of a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Found(String),
    /// No node has that id or that label.
    Unknown,
    /// More than one node carries that label, and the bus will not pick one.
    Ambiguous(usize),
}

impl Resolution {
    /// The id, or a refusal that names what the caller asked for.
    ///
    /// Both failures are [`AgentError::Refused`] rather than [`AgentError::NotConnected`]:
    /// *not connected* is a statement about two agents that both exist, and reporting it for
    /// a name nobody has would send the reader looking for a missing line on the board.
    pub fn into_result(self, query: &str) -> Result<String> {
        match self {
            Self::Found(id) => Ok(id),
            Self::Unknown => {
                Err(AgentError::Refused(format!("there is no agent called \"{query}\"")))
            }
            Self::Ambiguous(count) => Err(AgentError::Refused(format!(
                "{count} agents are called \"{query}\" — use the agent's id instead"
            ))),
        }
    }
}

/// One message, on its way.
///
/// Ids, not names: the bus resolves labels through [`Topology::resolve`] at the edge of the
/// system (the CLI, the IPC server) so that everything past that point is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub text: String,
    /// How many agent-to-agent hops this message is already the consequence of.
    ///
    /// **Zero for a message an agent sent of its own accord** — because the user asked it
    /// something, or a schedule woke it. An agent that is *answering* a message carries that
    /// message's count forward, which is the number [`Delivery::hops`] handed it. Getting
    /// this wrong is how a ping-pong loop escapes its bound while every unit test passes, so
    /// `agent_runtime.rs` must thread it through rather than reaching for `0`.
    pub hops: u32,
}

impl Message {
    /// A message an agent sends of its own accord: hop zero.
    pub fn new(from: impl Into<String>, to: impl Into<String>, text: impl Into<String>) -> Self {
        Self { from: from.into(), to: to.into(), text: text.into(), hops: 0 }
    }

    /// A message sent while handling another one — carrying that one's hop count.
    pub fn in_reply_at(mut self, hops: u32) -> Self {
        self.hops = hops;
        self
    }
}

/// One transcript line the app must append, and to which node.
///
/// The bus produces these instead of writing anything itself: transcripts are a sidecar the
/// app owns (§4), and a routing table that could also open files would be two responsibilities
/// and one of them untestable.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// The node whose transcript this belongs to.
    pub node: String,
    pub event: TranscriptEvent,
    /// The hop count the receiving session must carry into anything it sends in response.
    /// Meaningless on a refusal; carried anyway so the field is never `Option`.
    pub hops: u32,
}

/// A message that recently travelled a link, so the painter can pulse it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkPulse {
    /// True when the message went from the first id asked about to the second.
    pub forward: bool,
    /// 0.0 at the moment it was sent, 1.0 when the pulse is spent.
    pub progress: f32,
}

#[derive(Debug, Clone)]
struct Flight {
    from: String,
    to: String,
    until_ms: u64,
}

struct Inner {
    topology: Topology,
    queue: VecDeque<Delivery>,
    /// Every link with a live pulse on it. Empty on an idle board, which is what makes
    /// [`Bus::pulse`] a length check for the frame that matters — the one where nothing is
    /// happening.
    flights: Vec<Flight>,
}

/// The in-process router.
///
/// Shared: the frame loop drains it, and the IPC server's thread sends into it, so every
/// method takes `&self` and the state is behind a `Mutex`. **A poisoned lock is recovered
/// rather than panicked on** — a thread that died mid-send must not take the board's
/// messaging with it for the rest of the session, and every field in here is a plain
/// container with no invariant a partial write could break.
pub struct Bus {
    inner: Mutex<Inner>,
    max_hops: u32,
}

impl Bus {
    /// The default hop bound, from §6.
    ///
    /// Eight: deep enough that a genuine chain — planner → builder → reviewer → builder →
    /// reviewer → planner — completes, shallow enough that a pair of agents talking past
    /// each other stops within a few seconds instead of filling a transcript overnight.
    pub const DEFAULT_MAX_HOPS: u32 = 8;

    /// How long a link keeps its pulse after a message goes down it, in milliseconds.
    ///
    /// The pulse is **evidence that a message passed**, not a claim about how long it took —
    /// delivery is a queue push and takes no time at all. A fixed, short window is what makes
    /// it obey feedback 21's rule: the app repaints for as long as something is actually
    /// happening and then stops, rather than running a timer while the board sits still.
    pub const PULSE_MS: u64 = 600;

    /// The most deliveries that may wait to be drained.
    ///
    /// ⚠ **The queue had no bound at all**, and the two things that fill it do not need the
    /// app's cooperation to keep going: every `send` pushes **two** entries, and the IPC
    /// server's thread calls `send` — so an agent in a loop, or a headless run where nothing
    /// calls [`Bus::drain`], grew this `VecDeque` until the machine gave out. The hop limit
    /// bounds one *chain* and says nothing about how many chains there are; the refusal it
    /// produces is itself a queued entry.
    ///
    /// **The oldest is dropped, not the newest**, which is the opposite of what a queue
    /// usually wants and is right here. This is drained once per frame, so reaching four
    /// thousand entries does not mean the app is behind — it means something is generating
    /// messages faster than a board can be read, and the recent end is the part that says
    /// what. Losing the start of a runaway conversation costs nothing; losing the end costs
    /// the only evidence of what it turned into.
    pub const MAX_QUEUED: usize = 4_096;

    pub fn new() -> Self {
        Self::with_max_hops(Self::DEFAULT_MAX_HOPS)
    }

    /// A bus with a different hop bound. Configurable per §6; the app reads it from
    /// preferences, and a value of zero means an agent may not message another at all.
    pub fn with_max_hops(max_hops: u32) -> Self {
        Self {
            inner: Mutex::new(Inner {
                topology: Topology::new(),
                queue: VecDeque::new(),
                flights: Vec::new(),
            }),
            max_hops,
        }
    }

    pub fn max_hops(&self) -> u32 {
        self.max_hops
    }

    /// Replace what the bus believes about the board. Called when connectors or agent nodes
    /// change; cheap enough to call on every change rather than diffing.
    pub fn set_topology(&self, topology: Topology) {
        self.lock().topology = topology;
    }

    /// Read the topology without cloning it.
    ///
    /// A closure rather than a returned guard so the lock cannot be held across a caller's
    /// own work by accident — the IPC thread and the frame loop both take it.
    pub fn with_topology<R>(&self, f: impl FnOnce(&Topology) -> R) -> R {
        f(&self.lock().topology)
    }

    /// Turn a name or an id into an id. See [`Topology::resolve`].
    pub fn resolve(&self, query: &str) -> Resolution {
        self.lock().topology.resolve(query)
    }

    /// Route one message.
    ///
    /// Returns as soon as the delivery is queued; see the module header on why it can never
    /// wait for the receiver.
    ///
    /// # Refusals, and who hears about them
    ///
    /// Every refusal returns `Err` **and** most of them queue a transcript line, and the
    /// split is deliberate. `Err` is for the caller that is standing there — the CLI process
    /// waiting on a socket, or the session that called `send` — so it can adapt. The queued
    /// line is for the person who later opens the board and asks why nothing happened.
    ///
    /// | refusal | `Err` | transcript line |
    /// |---|---|---|
    /// | no connector | [`AgentError::NotConnected`] | none — nothing happened on either node |
    /// | not an agent node | [`AgentError::Refused`] | none |
    /// | hop limit | [`AgentError::Refused`] | on the **receiver**, per §6 |
    /// | muted receiver | [`AgentError::Refused`] | on the **sender** |
    ///
    /// The hop limit reports on the receiver because that is where a loop is visible: the
    /// node being hammered is the one a user clicks on. Muting reports on the sender for the
    /// opposite reason — a muted agent asked not to be interrupted, and writing into its
    /// transcript would be interrupting it.
    ///
    /// **A caller that surfaces the `Err` must not also write its own transcript line**, or
    /// every refusal appears twice.
    pub fn send(&self, message: Message, now_ms: u64) -> Result<()> {
        let mut inner = self.lock();
        inner.expire(now_ms);

        let Message { from, to, text, hops } = message;

        let Some(from_name) = inner.topology.name_of(&from).map(str::to_owned) else {
            return Err(AgentError::Refused(format!("{from} is not an agent node")));
        };
        let Some(to_name) = inner.topology.name_of(&to).map(str::to_owned) else {
            return Err(AgentError::Refused(format!("{to} is not an agent node")));
        };

        if !inner.topology.may_send(&from, &to) {
            return Err(AgentError::NotConnected { from: from_name, to: to_name });
        }

        // The hop bound is checked before anything else that could refuse, so that no later
        // condition can shadow the one guarantee that stops a runaway loop.
        if hops >= self.max_hops {
            let reason = format!(
                "a message from {from_name} was refused: it is {hops} hops deep and the limit \
                 is {}. Two agents may be answering each other in a loop.",
                self.max_hops
            );
            // The line stays — §6 puts it on the receiver on purpose, because the node being
            // hammered is the one a user clicks on — but a *run* of identical refusals is one
            // line's worth of information, and a loop generates them as fast as it turns.
            if !inner.already_said(&to, &reason) {
                inner.enqueue(Delivery {
                    node: to,
                    event: TranscriptEvent::Error { message: reason.clone() },
                    hops,
                });
            }
            return Err(AgentError::Refused(reason));
        }

        if inner.topology.accepts(&to) != Some(true) {
            let reason = format!("{to_name} is not accepting messages");
            if !inner.already_said(&from, &reason) {
                inner.enqueue(Delivery {
                    node: from,
                    event: TranscriptEvent::Error { message: reason.clone() },
                    hops,
                });
            }
            return Err(AgentError::Refused(reason));
        }

        // Both transcripts, so a conversation reads correctly from either end.
        inner.enqueue(Delivery {
            node: from.clone(),
            event: TranscriptEvent::MessageSent {
                to: AgentRef::new(to.clone(), to_name),
                text: text.clone(),
            },
            hops,
        });
        inner.enqueue(Delivery {
            node: to.clone(),
            event: TranscriptEvent::Message {
                from: AgentRef::new(from.clone(), from_name),
                text,
            },
            hops: hops + 1,
        });

        inner.light(&from, &to, now_ms + Self::PULSE_MS);
        Ok(())
    }

    /// Take everything waiting. Called once per frame; the app appends each event to that
    /// node's transcript and hands a [`TranscriptEvent::Message`] to the receiving session as
    /// its next turn's input.
    pub fn drain(&self) -> Vec<Delivery> {
        let mut inner = self.lock();
        inner.queue.drain(..).collect()
    }

    /// How many deliveries are waiting, without taking them. For the HUD.
    pub fn pending(&self) -> usize {
        self.lock().queue.len()
    }

    /// Whether a message recently went from `from` to `to`.
    pub fn in_flight(&self, from: &str, to: &str, now_ms: u64) -> bool {
        self.lock()
            .flights
            .iter()
            .any(|flight| flight.from == from && flight.to == to && flight.until_ms > now_ms)
    }

    /// The pulse to draw on the connector between `a` and `b`, in either direction.
    ///
    /// Asked once per frame per **visible** connector, so it is a scan over a vector that is
    /// empty on an idle board and holds one or two entries on a busy one. That is cheaper
    /// than the hash of a two-string key, and it is why the flights are a `Vec`.
    pub fn pulse(&self, a: &str, b: &str, now_ms: u64) -> Option<LinkPulse> {
        let inner = self.lock();
        inner.flights.iter().find_map(|flight| {
            let forward = flight.from == a && flight.to == b;
            let backward = flight.from == b && flight.to == a;
            if (!forward && !backward) || flight.until_ms <= now_ms {
                return None;
            }
            let left = flight.until_ms.saturating_sub(now_ms) as f32;
            let progress = (1.0 - left / Self::PULSE_MS as f32).clamp(0.0, 1.0);
            Some(LinkPulse { forward, progress })
        })
    }

    /// Whether anything at all is pulsing — the painter's early-out, so a board with fifty
    /// connectors on screen and nothing happening asks one question rather than fifty.
    pub fn any_in_flight(&self, now_ms: u64) -> bool {
        self.lock().flights.iter().any(|flight| flight.until_ms > now_ms)
    }

    /// Drop expired pulses. Called from `send` and `drain` anyway; exposed so a frame that
    /// does neither still lets the vector return to empty.
    pub fn expire(&self, now_ms: u64) {
        self.lock().expire(now_ms);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    fn expire(&mut self, now_ms: u64) {
        self.flights.retain(|flight| flight.until_ms > now_ms);
    }

    /// Queues a delivery, bounded by [`Bus::MAX_QUEUED`].
    ///
    /// **The one way anything reaches the queue**, so the bound cannot be forgotten at one of
    /// the four call sites — which is exactly how the queue came to be unbounded at all of
    /// them. See the constant for why the *oldest* is what goes.
    fn enqueue(&mut self, delivery: Delivery) {
        while self.queue.len() >= Bus::MAX_QUEUED {
            self.queue.pop_front();
        }
        self.queue.push_back(delivery);
    }

    /// Whether the tail of the queue is already this exact refusal for this exact node.
    ///
    /// A hop-limit refusal is generated at the speed of the loop that provokes it, so two
    /// agents answering each other produce a run of identical lines. The bound above is what
    /// actually protects the memory; this stops the *transcript* from being a thousand copies
    /// of one sentence, which is the difference between a message a user reads and one they
    /// scroll past. Only the tail is checked, which is O(1) and is where a run repeats.
    fn already_said(&self, node: &str, message: &str) -> bool {
        matches!(
            self.queue.back(),
            Some(Delivery { node: at, event: TranscriptEvent::Error { message: said }, .. })
                if at.as_str() == node && said.as_str() == message
        )
    }

    /// Light a link, or extend the pulse already on it — a second message down the same wire
    /// must not restart an animation that is halfway through, it must keep it alive.
    fn light(&mut self, from: &str, to: &str, until_ms: u64) {
        if let Some(flight) =
            self.flights.iter_mut().find(|f| f.from == from && f.to == to)
        {
            flight.until_ms = flight.until_ms.max(until_ms);
            return;
        }
        self.flights.push(Flight {
            from: from.to_owned(),
            to: to.to_owned(),
            until_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two agents, wired both ways, both listening. The starting point for most of these.
    fn pair() -> Topology {
        let mut topology = Topology::new();
        topology.node("a", "Planner", true).node("b", "Builder", true);
        topology.link("a", "b", LinkDirection::Both);
        topology
    }

    fn bus_with(topology: Topology) -> Bus {
        let bus = Bus::new();
        bus.set_topology(topology);
        bus
    }

    /// §3's whole point. A connector is the user's permission; without one there is no
    /// conversation, and the refusal names both ends so the toast can say which line is
    /// missing.
    #[test]
    fn a_message_with_no_connector_between_the_two_agents_is_refused() {
        let mut topology = Topology::new();
        topology.node("a", "Planner", true).node("b", "Builder", true);
        // Deliberately no `link` call.
        let bus = bus_with(topology);

        let refusal = bus.send(Message::new("a", "b", "start"), 0).unwrap_err();
        assert!(
            matches!(refusal, AgentError::NotConnected { .. }),
            "an unconnected message was not refused as unconnected: {refusal}"
        );
        assert!(refusal.to_string().contains("Planner"), "{refusal}");
        assert!(refusal.to_string().contains("Builder"), "{refusal}");
        // Nothing happened on either node, so neither transcript gains a line.
        assert!(bus.drain().is_empty(), "a refused message still queued a delivery");
    }

    /// An arrowhead points the conversation. This is the assertion that would fail on a bus
    /// that treated every link as bidirectional — which is exactly what "just store the pair"
    /// would give, and it looks correct in every test that only ever sends one way.
    #[test]
    fn an_arrowhead_at_one_end_makes_the_link_one_way() {
        let mut topology = Topology::new();
        topology.node("a", "Planner", true).node("b", "Builder", true);
        topology.link("a", "b", LinkDirection::from_arrowheads(false, true));
        let bus = bus_with(topology);

        assert!(bus.send(Message::new("a", "b", "do this"), 0).is_ok());
        let back = bus.send(Message::new("b", "a", "done"), 0).unwrap_err();
        assert!(
            matches!(back, AgentError::NotConnected { .. }),
            "a one-way link carried a message backwards: {back}"
        );

        // Neither arrowhead, and both, are the same answer: bidirectional.
        assert_eq!(LinkDirection::from_arrowheads(false, false), LinkDirection::Both);
        assert_eq!(LinkDirection::from_arrowheads(true, true), LinkDirection::Both);
        assert_eq!(LinkDirection::from_arrowheads(true, false), LinkDirection::Backward);
    }

    /// A muted agent refuses, and **the sender is told** rather than the message being
    /// swallowed — so an orchestrator can hand the work to somebody else instead of waiting
    /// for a reply that was never going to come.
    #[test]
    fn a_muted_agent_refuses_and_the_sender_is_told_rather_than_the_message_vanishing() {
        let mut topology = Topology::new();
        topology.node("a", "Planner", true).node("b", "Builder", false);
        topology.link("a", "b", LinkDirection::Both);
        let bus = bus_with(topology);

        let refusal = bus.send(Message::new("a", "b", "start"), 0).unwrap_err();
        assert!(refusal.to_string().contains("Builder"), "{refusal}");

        let queued = bus.drain();
        assert_eq!(queued.len(), 1, "a muted send queued the wrong number of lines: {queued:?}");
        assert_eq!(queued[0].node, "a", "the refusal was written on the muted node");
        assert!(matches!(queued[0].event, TranscriptEvent::Error { .. }));
        assert!(
            !bus.any_in_flight(0),
            "a refused message lit the connector, so the board would pulse for a message \
             that never left"
        );
    }

    /// Two agents pointed at each other, answering each other, forever. The loop below is
    /// bounded at 100 so a bus that never refuses **fails the final assertion** rather than
    /// hanging the test suite — a hang reads as CI being slow, and this is precisely the
    /// failure that would be shrugged at.
    #[test]
    fn two_agents_pointed_at_each_other_stop_at_the_hop_limit() {
        let bus = bus_with(pair());

        let mut hops = 0;
        let mut delivered = 0;
        let mut refused = None;
        for turn in 0..100 {
            let (from, to) = if turn % 2 == 0 { ("a", "b") } else { ("b", "a") };
            match bus.send(Message::new(from, to, "again").in_reply_at(hops), 0) {
                Ok(()) => {
                    delivered += 1;
                    // The receiving session carries forward what the delivery handed it —
                    // this is the arithmetic `agent_runtime.rs` must reproduce.
                    let arrived = bus
                        .drain()
                        .into_iter()
                        .find(|d| matches!(d.event, TranscriptEvent::Message { .. }))
                        .expect("a delivered message produced no line on the receiver");
                    hops = arrived.hops;
                }
                Err(error) => {
                    refused = Some(error);
                    break;
                }
            }
        }

        let refusal = refused.expect("the loop never stopped — the hop bound did nothing");
        assert_eq!(
            delivered,
            Bus::DEFAULT_MAX_HOPS as usize,
            "the loop ran {delivered} times against a limit of {}",
            Bus::DEFAULT_MAX_HOPS
        );
        assert!(refusal.to_string().contains("loop"), "{refusal}");

        // §6: the refusal is reported on the receiving node rather than dropped silently.
        let reported = bus.drain();
        assert_eq!(reported.len(), 1, "{reported:?}");
        // Turn 8 is even, so the refused message was aimed at "b" — and "b" is where §6 says
        // the refusal is recorded, because the node being hammered is the one a user clicks.
        assert_eq!(reported[0].node, "b", "the hop-limit refusal landed on the wrong node");
        assert!(matches!(reported[0].event, TranscriptEvent::Error { .. }));
    }

    /// ⚠ **Nothing bounded the queue.** Every `send` pushes *two* entries, the IPC server's
    /// thread calls `send`, and the only thing that empties it is the app's frame loop — so an
    /// agent in a loop, or any run where nothing drains, grew this `VecDeque` without limit
    /// inside the application's own process. The hop limit bounds one *chain* and says nothing
    /// about how many chains there are, and its refusal is itself a queued entry.
    ///
    /// Both halves are asserted, because a cap that kept the wrong end would be worse than
    /// none: this is drained every frame, so a full queue means something is generating faster
    /// than a board can be read, and the recent end is the part that says what.
    #[test]
    fn the_delivery_queue_is_bounded_and_keeps_the_newest() {
        let bus = bus_with(pair());
        // The cap in messages, so **twice** the cap in deliveries — every `send` pushes two —
        // with nothing draining in between.
        for index in 0..Bus::MAX_QUEUED {
            bus.send(Message::new("a", "b", format!("message {index}")), 0).unwrap();
        }
        assert_eq!(bus.pending(), Bus::MAX_QUEUED, "the queue grew past its bound");

        let drained = bus.drain();
        assert_eq!(drained.len(), Bus::MAX_QUEUED);
        // The *last* message sent must still be in there. Dropping the newest would mean a
        // runaway conversation is recorded only as the part before it went wrong.
        let last = format!("message {}", Bus::MAX_QUEUED - 1);
        assert!(
            drained.iter().any(|delivery| matches!(
                &delivery.event,
                TranscriptEvent::Message { text, .. } if *text == last
            )),
            "the newest delivery was dropped"
        );
        assert!(bus.pending() == 0, "drain left something behind");
    }

    /// A hop-limit refusal is generated at the speed of the loop that provokes it, so a pair
    /// of agents talking past each other writes the same sentence into one transcript over and
    /// over. The bound above is what protects the memory; this is what stops the transcript
    /// from being a thousand copies of one line, which is the difference between a message a
    /// user reads and one they scroll past.
    #[test]
    fn a_run_of_identical_refusals_is_recorded_once() {
        let bus = Bus::with_max_hops(0);
        bus.set_topology(pair());

        for _ in 0..50 {
            assert!(bus.send(Message::new("a", "b", "hello"), 0).is_err());
        }
        let queued = bus.drain();
        assert_eq!(queued.len(), 1, "one refusal repeated 50 times wrote 50 lines: {queued:?}");
        assert_eq!(queued[0].node, "b");

        // Not a *global* suppression, which would be the wrong fix: only a refusal that is
        // already the last thing in the queue is skipped, so the same refusal after other
        // traffic still gets its line.
        let mut muted = Topology::new();
        muted.node("a", "Planner", true).node("b", "Builder", false);
        muted.link("a", "b", LinkDirection::Both);
        let bus = bus_with(muted);

        assert!(bus.send(Message::new("a", "b", "one"), 0).is_err());
        assert!(bus.send(Message::new("a", "b", "two"), 0).is_err());
        assert_eq!(bus.pending(), 1, "the same refusal twice in a row is still one line");

        bus.set_topology(pair()); // "b" accepts again
        bus.send(Message::new("a", "b", "three"), 0).unwrap();
        assert_eq!(bus.pending(), 3);

        let mut muted = Topology::new();
        muted.node("a", "Planner", true).node("b", "Builder", false);
        muted.link("a", "b", LinkDirection::Both);
        bus.set_topology(muted);
        assert!(bus.send(Message::new("a", "b", "four"), 0).is_err());
        assert_eq!(bus.pending(), 4, "a refusal after other traffic was swallowed");
    }

    /// The bound is configurable, and zero means "these agents may not talk at all" rather
    /// than "one free hop".
    #[test]
    fn the_hop_bound_is_configurable_down_to_nothing() {
        let bus = Bus::with_max_hops(0);
        bus.set_topology(pair());
        assert!(bus.send(Message::new("a", "b", "hello"), 0).is_err());

        let bus = Bus::with_max_hops(1);
        bus.set_topology(pair());
        assert!(bus.send(Message::new("a", "b", "hello"), 0).is_ok());
        assert!(bus.send(Message::new("a", "b", "hello").in_reply_at(1), 0).is_err());
    }

    /// One message, two transcripts, and they agree. The sender's line names the receiver and
    /// the receiver's names the sender — a bus that wrote only the receiver's would leave an
    /// orchestrator's own transcript silent about everything it had delegated.
    #[test]
    fn both_transcripts_tell_the_truth_about_one_message() {
        let bus = bus_with(pair());
        bus.send(Message::new("a", "b", "take this"), 0).unwrap();

        let queued = bus.drain();
        assert_eq!(queued.len(), 2, "{queued:?}");

        let sent = queued.iter().find(|d| d.node == "a").expect("no line on the sender");
        match &sent.event {
            TranscriptEvent::MessageSent { to, text } => {
                assert_eq!(to.item, "b");
                assert_eq!(to.name, "Builder", "the sender's line did not name the receiver");
                assert_eq!(text, "take this");
            }
            other => panic!("the sender got {other:?}"),
        }

        let got = queued.iter().find(|d| d.node == "b").expect("no line on the receiver");
        match &got.event {
            TranscriptEvent::Message { from, text } => {
                assert_eq!(from.item, "a");
                assert_eq!(from.name, "Planner");
                assert_eq!(text, "take this");
            }
            other => panic!("the receiver got {other:?}"),
        }
        assert_eq!(got.hops, 1, "the delivered hop count did not advance");
        assert_eq!(sent.hops, 0, "the sender's own line should record the hop it sent at");
    }

    /// The pulse exists so the connector animates while a message passes, and **stops** —
    /// feedback 21's rule. A window that never expired would look identical in a screenshot
    /// and repaint at 60Hz forever, so the assertion is about the far side of the window.
    #[test]
    fn a_pulse_expires_so_an_idle_board_asks_for_no_repaints() {
        let bus = bus_with(pair());
        assert!(!bus.any_in_flight(0), "an untouched board had something in flight");

        bus.send(Message::new("a", "b", "go"), 1_000).unwrap();
        assert!(bus.in_flight("a", "b", 1_000));
        assert!(!bus.in_flight("b", "a", 1_000), "the pulse ran the wrong way down the link");

        let pulse = bus.pulse("a", "b", 1_000).expect("no pulse on a link that just carried");
        assert!(pulse.forward);
        assert!(pulse.progress < 0.01, "a fresh pulse started part-way through: {pulse:?}");

        let halfway = bus.pulse("a", "b", 1_000 + Bus::PULSE_MS / 2).expect("pulse died early");
        assert!((0.4..0.6).contains(&halfway.progress), "{halfway:?}");

        // Asked from the other end, the same pulse reports itself as running backwards, so
        // the painter can animate one connector without knowing which way the user drew it.
        let reversed = bus.pulse("b", "a", 1_000 + 100).expect("the link had no pulse");
        assert!(!reversed.forward);

        let after = 1_000 + Bus::PULSE_MS + 1;
        assert!(!bus.any_in_flight(after), "the pulse outlived its window");
        assert_eq!(bus.pulse("a", "b", after), None);

        bus.expire(after);
        assert!(!bus.any_in_flight(after));
    }

    /// Sending is a queue push. Nothing here drains, and every send still succeeds — which
    /// is the property that keeps a send off the critical path of a receiving agent that is
    /// mid-turn, asleep, or not started.
    #[test]
    fn sending_never_waits_for_the_receiver_to_be_drained() {
        let bus = bus_with(pair());
        for i in 0..50 {
            bus.send(Message::new("a", "b", format!("message {i}")), 0).unwrap();
        }
        assert_eq!(bus.pending(), 100, "50 messages should be 50 sent lines and 50 received");
        assert_eq!(bus.drain().len(), 100);
        assert_eq!(bus.pending(), 0);
    }

    /// An agent knows its neighbour by the label on the board. Two agents wearing the same
    /// label is ordinary, and picking one by hash-map order would deliver to a different
    /// agent on different runs of the same board.
    #[test]
    fn a_name_resolves_to_a_node_and_an_ambiguous_one_is_refused_rather_than_guessed() {
        let mut topology = Topology::new();
        topology
            .node("a", "Planner", true)
            .node("b", "Builder", true)
            .node("c", "Builder", true);

        assert_eq!(topology.resolve("a"), Resolution::Found("a".into()));
        assert_eq!(topology.resolve("Planner"), Resolution::Found("a".into()));
        assert_eq!(topology.resolve("planner"), Resolution::Found("a".into()));
        assert_eq!(topology.resolve("  Planner "), Resolution::Found("a".into()));
        assert_eq!(topology.resolve("Builder"), Resolution::Ambiguous(2));
        assert_eq!(topology.resolve("Reviewer"), Resolution::Unknown);

        let unknown = topology.resolve("Reviewer").into_result("Reviewer").unwrap_err();
        assert!(unknown.to_string().contains("Reviewer"), "{unknown}");
        let ambiguous = topology.resolve("Builder").into_result("Builder").unwrap_err();
        assert!(ambiguous.to_string().contains("id"), "{ambiguous}");
    }

    /// A note, a file tree and a frame are all things a connector can reach. Only agent
    /// nodes are registered, so a message aimed at one of the others is refused as *not an
    /// agent* — a different sentence from *not connected*, and the difference is what stops
    /// the user looking for a line that is right there.
    #[test]
    fn a_message_aimed_at_something_that_is_not_an_agent_is_refused_by_name() {
        let mut topology = Topology::new();
        topology.node("a", "Planner", true);
        topology.allow("a", "note");
        let bus = bus_with(topology);

        let refusal = bus.send(Message::new("a", "note", "hello"), 0).unwrap_err();
        assert!(refusal.to_string().contains("not an agent node"), "{refusal}");
        assert!(bus.drain().is_empty());
    }

    /// A connector from an agent to itself is a legal thing to draw and an infinite loop to
    /// honour. It is dropped when the topology is built, so the hop bound never has to be
    /// the thing that catches it.
    #[test]
    fn an_agent_cannot_message_itself() {
        let mut topology = Topology::new();
        topology.node("a", "Planner", true);
        topology.link("a", "a", LinkDirection::Both);
        assert!(!topology.may_send("a", "a"));

        let bus = bus_with(topology);
        assert!(bus.send(Message::new("a", "a", "hello"), 0).is_err());
    }

    /// The topology is rebuilt rather than patched, so rebuilding must be idempotent and a
    /// node's state must follow the rebuild — muting an agent has to take effect on the next
    /// message, not the next launch.
    #[test]
    fn rebuilding_the_topology_replaces_what_the_bus_believes() {
        let bus = bus_with(pair());
        assert!(bus.send(Message::new("a", "b", "one"), 0).is_ok());

        let mut muted = Topology::new();
        muted.node("a", "Planner", true).node("b", "Builder", false);
        muted.link("a", "b", LinkDirection::Both);
        bus.set_topology(muted);

        assert!(bus.send(Message::new("a", "b", "two"), 0).is_err());
        assert_eq!(bus.with_topology(Topology::len), 2);
        // Joined into a `String` rather than returned as `Vec<&str>`: the closure is handed a
        // reference borrowed from the lock, so nothing it returns may borrow from it.
        assert_eq!(bus.with_topology(|t| t.targets("a").join(",")), "b");
    }
}
