//! What happened while you were not watching.
//!
//! Agents keep running when the window is behind something else, so coming back to a board
//! means coming back to several transcripts nobody read. Feature 15 is the answer to that:
//! a digest that says, per agent, what it did, what changed, and **what needs you** — rather
//! than making the user scroll raw logs looking for the one agent that stopped.
//!
//! # The ranking is the feature
//!
//! An unanswered permission request, an error, a failed turn, an agent still mid-task: those
//! come first and are **never collapsed**. Everything else is compressed — forty tool calls
//! become *"40 tools (20 read, 12 edit, …)"*, because the individual calls are the part the
//! user chose Clean mode to avoid. A digest that buries a blocked agent under twelve lines of
//! *"ran a tool"* is a digest nobody reads twice, and after that the feature is worse than
//! nothing: it is a thing that looked like it was watching.
//!
//! [`Attention`]'s **declaration order is the ranking** and everything sorts by it, so there
//! is one definition of "urgent" rather than one per rendering.
//!
//! # Deciding *when* the user was away is not this module's
//!
//! `since` is a parameter. Window focus is `vellum-app`'s — this crate does not read a clock
//! (see the crate doc), and it certainly does not know whether a window is frontmost. What it
//! promises in return is that the same events and the same `since` always give the same
//! digest.
//!
//! # `since` bounds what is *new*, not what is *blocked*
//!
//! One pass, two scopes, and this is the correction that makes the ranking honest:
//!
//! - **State** — unanswered permission requests, an option set nobody picked from, a turn
//!   that opened and never ended — is derived from **every event the caller passes**. A
//!   permission asked an hour before the user left and still unanswered is an agent that has
//!   been blocked the whole time they were away, and a hard `at >= since` filter would report
//!   that board as all quiet. That is precisely the failure this module exists to prevent.
//! - **Counts and prose** — the tool tally, what the agent said, failures — are bounded by
//!   `at >= since`, because they answer *"what is new"*.
//!
//! So the caller passes the transcript tail from as far back as it is willing to read (the
//! sidecar is append-only JSONL and cheap to tail), and `since` decides what counts as news.
//!
//! # Two forms, one derivation
//!
//! [`AgentDigest::line`] is a card on the board; [`AgentDigest::panel`] is the inspector.
//! Both read the same [`AgentDigest`], and both put the counts on screen through
//! [`Activity::brief`] and [`Activity::phrase`], which are two renderings of one set of
//! numbers. Two summaries that could disagree about how many tools an agent ran would make
//! the user check the log, which is the thing the digest exists to replace.

use crate::{AgentRef, Timestamp, TranscriptEvent, TurnId, TurnOutcome};

/// Prose lines kept per agent. The **most recent** ones: an agent's last word is what a
/// person coming back is looking for, and the first is usually *"I'll start by reading…"*.
const MAX_PROSE: usize = 5;

/// A prose line's length, in characters.
const PROSE_CHARS: usize = 120;

/// An attention line's length, in characters. Shorter than prose because it is a label for
/// something to act on, not the thing itself.
const ATTENTION_CHARS: usize = 100;

/// Distinct tool names named in the collapsed phrase, before the ellipsis.
const TOP_TOOLS: usize = 3;

/// How urgently an agent wants the user.
///
/// **The declaration order is the ranking**, and every sort in this module is
/// `sort_by_key(|item| item.level)` against it. Deriving `Ord` rather than writing a
/// `rank()` function is deliberate: a second definition of urgency is a second thing that can
/// disagree with the first, and this repo has paid for exactly that twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attention {
    /// Stopped, waiting for the user: an unanswered permission request, or an option set
    /// nobody has picked from. Nothing outranks this — the agent cannot proceed at all, and
    /// every second it waits is wasted.
    Blocked,
    /// Something failed: an error, or a turn that ended badly.
    Failed,
    /// A turn opened and never ended in the events given.
    ///
    /// **This is "still mid-task", not "dead".** The events alone cannot tell a working agent
    /// from a dead session — only `vellum-app` knows whether the process is still there — so
    /// the wording promises nothing it cannot support, and the app is free to suppress this
    /// for a live session or upgrade it to [`Attention::Failed`] for one that is gone.
    Stalled,
    /// Nothing wanted the user. Ranked last, so it sorts below everything that does.
    Fine,
}

impl Attention {
    pub const ALL: [Self; 4] = [Self::Blocked, Self::Failed, Self::Stalled, Self::Fine];

    /// Whether this is something to put in front of the user.
    pub const fn wants_the_user(self) -> bool {
        !matches!(self, Self::Fine)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "waiting for you",
            Self::Failed => "something failed",
            Self::Stalled => "still mid-task",
            Self::Fine => "nothing needed",
        }
    }
}

/// One reason an agent needs the user, in the words the user reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionItem {
    pub level: Attention,
    pub text: String,
}

/// What an agent did, collapsed.
///
/// Counts rather than events: this is the half of the digest that is allowed to lose detail,
/// and losing it is the point. The transcript is still on disk for anyone who wants the
/// forty lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    /// Turns that **ended** in the window. A turn that is still open is reported as
    /// [`Attention::Stalled`] instead, so it is not quietly counted as work that finished.
    pub turns_finished: u32,
    /// Turns that started in the window, whether or not they ended.
    pub turns_started: u32,
    pub tools: u32,
    /// Per tool name, **count descending then name ascending** — a total order, so two runs
    /// over the same transcript render the same sentence.
    pub by_tool: Vec<(String, u32)>,
    /// Tools that answered `ok: false`. Counted and **not** treated as attention: an agent
    /// that greps for something absent has a failed tool and is perfectly fine. What matters
    /// is how the *turn* ended.
    pub tool_failures: u32,
    pub images: u32,
    pub options_offered: u32,
    pub messages_sent: u32,
    pub messages_received: u32,
}

impl Activity {
    /// Whether nothing worth reporting happened.
    pub const fn is_quiet(&self) -> bool {
        self.turns_finished == 0
            && self.turns_started == 0
            && self.tools == 0
            && self.images == 0
            && self.options_offered == 0
            && self.messages_sent == 0
            && self.messages_received == 0
    }

    /// The compact form, for a card: `"3 turns · 40 tools · 2 messages sent"`.
    pub fn brief(&self) -> String {
        let parts = self.parts(false);
        if parts.is_empty() { "no activity".into() } else { parts.join(" · ") }
    }

    /// The fuller form, for a panel:
    /// `"3 turns, 40 tools (20 read, 12 edit, 8 bash), 2 messages sent"`.
    ///
    /// Same numbers as [`Activity::brief`], through the same `parts` — which is what makes
    /// the card and the panel unable to disagree about how many tools an agent ran.
    pub fn phrase(&self) -> String {
        let parts = self.parts(true);
        if parts.is_empty() { "did nothing".into() } else { parts.join(", ") }
    }

    /// The one derivation both forms render.
    fn parts(&self, detailed: bool) -> Vec<String> {
        let mut parts = Vec::new();
        if self.turns_finished > 0 {
            parts.push(count(u64::from(self.turns_finished), "turn"));
        }
        if self.tools > 0 {
            let mut tools = count(u64::from(self.tools), "tool");
            if detailed && !self.by_tool.is_empty() {
                tools.push_str(" (");
                tools.push_str(&self.breakdown());
                tools.push(')');
            }
            parts.push(tools);
        }
        if detailed && self.tool_failures > 0 {
            parts.push(format!("{} of them failed", self.tool_failures));
        }
        if self.images > 0 {
            parts.push(count(u64::from(self.images), "image"));
        }
        if self.options_offered > 0 {
            parts.push(format!("{} offered", count(u64::from(self.options_offered), "option")));
        }
        if self.messages_sent > 0 {
            parts.push(format!("{} sent", count(u64::from(self.messages_sent), "message")));
        }
        if self.messages_received > 0 {
            parts.push(format!(
                "{} received",
                count(u64::from(self.messages_received), "message")
            ));
        }
        parts
    }

    /// `"20 read, 12 edit, 8 bash, …"` — the top few by count, with the ellipsis only when
    /// something was actually left out.
    fn breakdown(&self) -> String {
        let mut text = self
            .by_tool
            .iter()
            .take(TOP_TOOLS)
            .map(|(name, times)| format!("{times} {name}"))
            .collect::<Vec<_>>()
            .join(", ");
        if self.by_tool.len() > TOP_TOOLS {
            text.push_str(", …");
        }
        text
    }
}

/// One agent's share of the digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDigest {
    pub agent: AgentRef,
    /// The most urgent of [`AgentDigest::attention_items`], or [`Attention::Fine`].
    pub attention: Attention,
    /// Why the user is wanted, most urgent first. **Never collapsed** — this is the list the
    /// whole module exists to keep visible.
    pub attention_items: Vec<AttentionItem>,
    pub activity: Activity,
    /// What the agent actually said, most recent last. Prose is what gets shown; the tool
    /// calls that produced it are the part that collapses.
    pub said: Vec<String>,
    /// Prose lines dropped to keep [`AgentDigest::said`] bounded, so the panel can say so
    /// rather than silently pretending the agent was brief.
    pub said_dropped: u32,
    pub first_activity: Option<Timestamp>,
    pub last_activity: Option<Timestamp>,
}

impl AgentDigest {
    /// Read one agent's transcript tail into a digest.
    ///
    /// `events` are `(when it was recorded, what it was)` pairs — [`TranscriptEvent`] carries
    /// no timestamp of its own, because the sidecar records when each line was appended and
    /// duplicating that inside the event would give a JSONL file two answers to one question.
    ///
    /// Pass **more** than the away period: see the module doc on why state is derived from
    /// everything given while counts are bounded by `since`.
    pub fn build<'a>(
        agent: AgentRef,
        since: Timestamp,
        events: impl IntoIterator<Item = (Timestamp, &'a TranscriptEvent)>,
    ) -> Self {
        let mut activity = Activity::default();
        let mut said: Vec<String> = Vec::new();
        let mut said_dropped = 0;
        let mut first_activity = None;
        let mut last_activity = None;

        // State, from the whole stream.
        let mut open_turns: Vec<(TurnId, String)> = Vec::new();
        let mut unanswered: Vec<(&crate::RequestId, String)> = Vec::new();
        let mut awaiting_choice: Option<String> = None;
        // News, from the window.
        let mut failures: Vec<String> = Vec::new();

        for (at, event) in events {
            let fresh = at >= since;
            if fresh {
                first_activity.get_or_insert(at);
                last_activity = Some(at);
            }

            match event {
                TranscriptEvent::TurnStarted { turn, prompt } => {
                    if fresh {
                        activity.turns_started += 1;
                    }
                    open_turns.push((*turn, clip(prompt, ATTENTION_CHARS)));
                }
                TranscriptEvent::TurnEnded { turn, outcome } => {
                    open_turns.retain(|(open, _)| open != turn);
                    if fresh {
                        activity.turns_finished += 1;
                        match outcome {
                            TurnOutcome::Failed { message } => failures
                                .push(format!("failed: {}", clip(message, ATTENTION_CHARS))),
                            TurnOutcome::Exhausted { message } => failures.push(format!(
                                "stopped short: {}",
                                clip(message, ATTENTION_CHARS)
                            )),
                            // Cancelled is the user's own doing, and reporting someone's
                            // own click back to them as a problem is noise.
                            TurnOutcome::Completed | TurnOutcome::Cancelled => {}
                        }
                    }
                }
                TranscriptEvent::ToolCall { name, .. } => {
                    if fresh {
                        activity.tools += 1;
                        bump(&mut activity.by_tool, name);
                    }
                }
                TranscriptEvent::ToolResult { ok: false, .. } => {
                    if fresh {
                        activity.tool_failures += 1;
                    }
                }
                TranscriptEvent::Text { text } => {
                    if fresh {
                        let line = clip(text, PROSE_CHARS);
                        if !line.is_empty() {
                            said.push(line);
                            if said.len() > MAX_PROSE {
                                said.remove(0);
                                said_dropped += 1;
                            }
                        }
                    }
                }
                TranscriptEvent::Image { .. } => {
                    if fresh {
                        activity.images += 1;
                    }
                }
                TranscriptEvent::Options { prompt, chosen, .. } => {
                    if fresh {
                        activity.options_offered += 1;
                    }
                    // The last offer standing is what is on the node. An `Options` carries
                    // no id of its own, so a later answered set clears an earlier unanswered
                    // one — which matches what the user sees, and is stated here rather than
                    // pretended to be exact.
                    awaiting_choice = match chosen {
                        None => Some(clip(prompt, ATTENTION_CHARS)),
                        Some(_) => None,
                    };
                }
                TranscriptEvent::PermissionRequest { id, summary, .. } => {
                    unanswered.push((id, clip(summary, ATTENTION_CHARS)));
                }
                TranscriptEvent::PermissionAnswer { id, .. } => {
                    unanswered.retain(|(asked, _)| *asked != id);
                }
                TranscriptEvent::Message { .. } => {
                    if fresh {
                        activity.messages_received += 1;
                    }
                }
                TranscriptEvent::MessageSent { .. } => {
                    if fresh {
                        activity.messages_sent += 1;
                    }
                }
                TranscriptEvent::Error { message } => {
                    if fresh {
                        failures.push(format!("error: {}", clip(message, ATTENTION_CHARS)));
                    }
                }
                // Deliberately uncounted. A thought is scaffolding, and terminal bytes
                // arrive by the thousand from a PTY that is doing nothing but drawing a
                // spinner — counting either would make an idle agent look busy.
                TranscriptEvent::Thought { .. }
                | TranscriptEvent::Terminal { .. }
                | TranscriptEvent::ToolResult { ok: true, .. } => {}
            }
        }

        activity.by_tool.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut attention_items = Vec::new();
        for (_, summary) in &unanswered {
            attention_items.push(AttentionItem {
                level: Attention::Blocked,
                text: format!("waiting for permission: {summary}"),
            });
        }
        if let Some(prompt) = &awaiting_choice {
            attention_items.push(AttentionItem {
                level: Attention::Blocked,
                text: format!("waiting for you to choose: {prompt}"),
            });
        }
        for failure in failures {
            attention_items.push(AttentionItem { level: Attention::Failed, text: failure });
        }
        for (_, prompt) in &open_turns {
            attention_items.push(AttentionItem {
                level: Attention::Stalled,
                text: format!("still mid-task: {prompt}"),
            });
        }
        // Stable, so the order within one level is the order things happened.
        attention_items.sort_by_key(|item| item.level);

        let attention =
            attention_items.first().map_or(Attention::Fine, |item| item.level);

        Self {
            agent,
            attention,
            attention_items,
            activity,
            said,
            said_dropped,
            first_activity,
            last_activity,
        }
    }

    /// One line, for a card on the board.
    ///
    /// The lead is the most urgent thing, then the agent's last word, then the counts — so
    /// the first thing read is the thing to act on. The counts follow on the same line, which
    /// is what makes this form and [`AgentDigest::panel`] checkably consistent.
    ///
    /// Clipping to the card's width is the painter's: this crate measures nothing.
    pub fn line(&self) -> String {
        let lead = match (self.attention_items.first(), self.said.last()) {
            (Some(item), _) => item.text.clone(),
            (None, Some(prose)) => format!("“{prose}”"),
            (None, None) => String::new(),
        };
        let body = if lead.is_empty() {
            self.activity.brief()
        } else if self.activity.is_quiet() {
            lead
        } else {
            format!("{lead} · {}", self.activity.brief())
        };
        format!("{} — {body}", self.agent.name)
    }

    /// The fuller form, for a panel. Plain text, one fact per line.
    ///
    /// `!` marks a line that wants the user and `-` one that does not. **ASCII on purpose**:
    /// a warning triangle is outside the plain sans faces this app bundles, and trap 10's
    /// lesson is that the one glyph whose job is to say *look here* is the worst one to draw
    /// as tofu.
    pub fn panel(&self) -> String {
        let mut out = String::new();
        if self.attention.wants_the_user() {
            out.push_str(&format!("{} — {}\n", self.agent.name, self.attention.label()));
        } else {
            out.push_str(&format!("{}\n", self.agent.name));
        }
        for item in &self.attention_items {
            out.push_str(&format!("  ! {}\n", item.text));
        }
        out.push_str(&format!("  - {}\n", self.activity.phrase()));
        for line in &self.said {
            out.push_str(&format!("  - “{line}”\n"));
        }
        if self.said_dropped > 0 {
            out.push_str(&format!(
                "  - and {} before that\n",
                count(u64::from(self.said_dropped), "line")
            ));
        }
        out
    }

    /// Whether this agent did nothing and wants nothing.
    pub fn is_quiet(&self) -> bool {
        self.activity.is_quiet() && self.attention_items.is_empty() && self.said.is_empty()
    }
}

/// Every agent's share, ranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    /// When the user stopped watching. **Supplied**, never decided here — window focus is
    /// `vellum-app`'s.
    pub since: Timestamp,
    /// When they came back.
    pub until: Timestamp,
    /// Ranked: anything needing the user first, then by most recent activity, then by name.
    pub agents: Vec<AgentDigest>,
}

impl Digest {
    pub const fn new(since: Timestamp, until: Timestamp) -> Self {
        Self { since, until, agents: Vec::new() }
    }

    /// Add one agent's transcript tail.
    ///
    /// The ranking is applied **here**, on every add, rather than in a `finish()` the caller
    /// has to remember. A two-phase builder whose second phase is what makes the answer
    /// correct is a trap: `self.agents` would be readable, plausible and in the wrong order
    /// for anyone who did not call it.
    pub fn add<'a>(
        &mut self,
        agent: AgentRef,
        events: impl IntoIterator<Item = (Timestamp, &'a TranscriptEvent)>,
    ) {
        self.agents.push(AgentDigest::build(agent, self.since, events));
        self.agents.sort_by(|a, b| {
            a.attention
                .cmp(&b.attention)
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.agent.name.cmp(&b.agent.name))
        });
    }

    /// How long the user was away, in seconds.
    pub const fn span(&self) -> u64 {
        self.until.saturating_sub(self.since)
    }

    /// Whether anything at all happened.
    pub fn is_quiet(&self) -> bool {
        self.agents.iter().all(AgentDigest::is_quiet)
    }

    pub fn needs_attention(&self) -> bool {
        self.agents.iter().any(|agent| agent.attention.wants_the_user())
    }

    pub fn attention_count(&self) -> usize {
        self.agents.iter().filter(|agent| agent.attention.wants_the_user()).count()
    }

    /// The one sentence at the top of both forms.
    pub fn headline(&self) -> String {
        if self.is_quiet() {
            return format!("Nothing happened while you were away ({}).", span_phrase(self.span()));
        }
        // The agents that *did* something, not every agent on the board. One busy agent
        // beside one that sat idle is not "2 agents worked", and a headline that inflates
        // the first number is a headline nobody trusts the second one in.
        let busy = self.agents.iter().filter(|agent| !agent.is_quiet()).count();
        let worked = count(busy as u64, "agent");
        let waiting = self.attention_count();
        if waiting == 0 {
            format!("{worked} worked while you were away ({}).", span_phrase(self.span()))
        } else {
            format!(
                "{worked} worked while you were away ({}) — {} you.",
                span_phrase(self.span()),
                if waiting == 1 {
                    "1 needs".to_string()
                } else {
                    format!("{waiting} need")
                }
            )
        }
    }

    /// One line per agent, for a card on the board.
    ///
    /// Never empty: an away period in which nothing happened produces the sentence saying so,
    /// because a card that renders as nothing at all is indistinguishable from a card that
    /// failed to render.
    pub fn card_lines(&self) -> Vec<String> {
        if self.agents.is_empty() || self.is_quiet() {
            return vec![self.headline()];
        }
        self.agents.iter().map(AgentDigest::line).collect()
    }

    /// The fuller form, for a panel.
    pub fn panel_text(&self) -> String {
        let mut out = self.headline();
        out.push('\n');
        for agent in &self.agents {
            out.push('\n');
            out.push_str(&agent.panel());
        }
        out
    }

    /// The digest as a prompt, for a model asked to write it up in prose.
    ///
    /// **Optional, and never on the path.** [`Digest::card_lines`] and [`Digest::panel_text`]
    /// are the primary rendering and they need nothing but this struct: an away summary that
    /// required an API call would fail exactly when the user was away because their network
    /// dropped, which is the one time it has to work.
    ///
    /// It carries the facts and asks for nothing to be added, because a summary that invents
    /// a completed task is worse than a list of counts.
    pub fn prompt(&self) -> String {
        format!(
            "Summarise the following for someone who has just come back to their board. \
             Lead with anything that needs them. Use only what is below — do not infer or \
             invent anything else.\n\n{}",
            self.panel_text()
        )
    }
}

/// Add one to a tool's tally, keeping first-seen order until the final sort.
fn bump(counts: &mut Vec<(String, u32)>, name: &str) {
    if let Some(entry) = counts.iter_mut().find(|(known, _)| known.as_str() == name) {
        entry.1 += 1;
    } else {
        counts.push((name.to_string(), 1));
    }
}

/// `"1 turn"` / `"3 turns"`. Every noun this module counts takes a plain `s`.
fn count(n: u64, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// A rough duration, for the headline. Deliberately coarse: *"2h 15m"* is what the sentence
/// is for, and *"2h 14m 51s"* is a stopwatch.
fn span_phrase(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        0 => "no time at all".into(),
        s if s < MINUTE => count(s, "second"),
        s if s < HOUR => count(s / MINUTE, "minute"),
        s if s < DAY => {
            let (hours, minutes) = (s / HOUR, (s % HOUR) / MINUTE);
            if minutes == 0 { count(hours, "hour") } else { format!("{hours}h {minutes}m") }
        }
        s => count(s / DAY, "day"),
    }
}

/// The first line of some text, trimmed and bounded to `limit` **characters**.
///
/// Characters and not bytes, and `chars().take(..)` rather than `text[..limit]`. Every string
/// this module clips is agent output or a page's own words — the least controlled data in the
/// application — and `strip_site_affix` aborted the whole process twice on exactly this shape
/// of bug (feedback 30), with `panic = "abort"` leaving nothing to catch.
///
/// The ellipsis marks either cut: too long, or there were more lines below.
fn clip(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let first = trimmed.lines().next().unwrap_or_default().trim();
    let had_more_lines = trimmed.lines().nth(1).is_some();
    let mut out: String = first.chars().take(limit).collect();
    if had_more_lines || out.chars().count() < first.chars().count() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Choice, RequestId, ToolCallId};

    const T0: Timestamp = 1_700_000_000;

    fn agent(name: &str) -> AgentRef {
        AgentRef::new(format!("{name}@1"), name)
    }

    fn tool(name: &str) -> TranscriptEvent {
        TranscriptEvent::ToolCall {
            id: ToolCallId(name.into()),
            name: name.into(),
            input: String::new(),
        }
    }

    fn said(text: &str) -> TranscriptEvent {
        TranscriptEvent::Text { text: text.into() }
    }

    fn asked(summary: &str) -> TranscriptEvent {
        TranscriptEvent::PermissionRequest {
            id: RequestId("p1".into()),
            summary: summary.into(),
            detail: String::new(),
        }
    }

    /// Every fixture uses one turn, so the id is a constant rather than a parameter.
    fn started(prompt: &str) -> TranscriptEvent {
        TranscriptEvent::TurnStarted { turn: TurnId(1), prompt: prompt.into() }
    }

    fn ended(outcome: TurnOutcome) -> TranscriptEvent {
        TranscriptEvent::TurnEnded { turn: TurnId(1), outcome }
    }

    /// Forty tool calls and a closing sentence — a busy agent that needs nothing.
    fn busy() -> Vec<(Timestamp, TranscriptEvent)> {
        let mut events = vec![(T0, started("refactor"))];
        for index in 0..20 {
            events.push((T0 + index, tool("read")));
        }
        for index in 0..12 {
            events.push((T0 + 20 + index, tool("edit")));
        }
        for index in 0..8 {
            events.push((T0 + 32 + index, tool("bash")));
        }
        events.push((T0 + 45, said("Refactor done; three files changed.")));
        events.push((T0 + 46, ended(TurnOutcome::Completed)));
        events
    }

    /// A dozen tool calls and then a permission request nobody answered.
    fn blocked() -> Vec<(Timestamp, TranscriptEvent)> {
        let mut events = Vec::new();
        for index in 0..12 {
            events.push((T0 + index, tool("read")));
        }
        events.push((T0 + 20, asked("write to src/main.rs")));
        events
    }

    fn feed(
        events: &[(Timestamp, TranscriptEvent)],
    ) -> impl Iterator<Item = (Timestamp, &TranscriptEvent)> {
        events.iter().map(|(at, event)| (*at, event))
    }

    /// The whole point of the module: the blocked agent comes first, whatever order the
    /// agents were added in and however much noise is on top of it.
    ///
    /// **The busy agent is added first on purpose.** A build that never sorted would answer
    /// "Builder" here and pass any assertion that only checked the blocked agent's own level.
    #[test]
    fn an_agent_that_needs_the_user_comes_first_however_it_was_added() {
        let (busy, blocked) = (busy(), blocked());
        let mut digest = Digest::new(T0, T0 + 3600);
        digest.add(agent("Builder"), feed(&busy));
        digest.add(agent("Reviewer"), feed(&blocked));

        assert_eq!(
            digest.agents[0].agent.name,
            "Reviewer",
            "a busy agent outranked a blocked one"
        );
        assert_eq!(digest.agents[0].attention, Attention::Blocked);
        assert_eq!(digest.agents[1].attention, Attention::Fine);
        assert!(digest.needs_attention());
        assert_eq!(digest.attention_count(), 1);

        // And it is the *first* line of the card, not a line the user has to find.
        let lines = digest.card_lines();
        assert!(
            lines[0].starts_with("Reviewer — waiting for permission: write to src/main.rs"),
            "{}",
            lines[0]
        );
        assert!(digest.headline().contains("1 needs you"), "{}", digest.headline());
    }

    /// Ranked by level, and the levels are the declaration order. If this ever inverts, a
    /// blocked agent sorts below one that merely finished.
    #[test]
    fn the_levels_rank_in_the_order_they_are_declared() {
        assert!(Attention::Blocked < Attention::Failed);
        assert!(Attention::Failed < Attention::Stalled);
        assert!(Attention::Stalled < Attention::Fine);
        for level in Attention::ALL {
            assert_eq!(level.wants_the_user(), level != Attention::Fine);
        }
    }

    /// Forty tool calls become one phrase with the right numbers in it — and the tool
    /// breakdown is ordered by count, not by first appearance, so the sentence is the same
    /// on every run.
    #[test]
    fn repetition_collapses_with_the_counts_intact() {
        let busy = busy();
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&busy));

        assert_eq!(digest.activity.tools, 40);
        assert_eq!(
            digest.activity.by_tool,
            vec![("read".to_string(), 20), ("edit".to_string(), 12), ("bash".to_string(), 8)]
        );
        assert_eq!(digest.activity.turns_finished, 1);

        let phrase = digest.activity.phrase();
        assert!(phrase.contains("40 tools"), "{phrase}");
        assert!(phrase.contains("20 read"), "{phrase}");
        assert!(phrase.contains("1 turn,"), "{phrase}");
        // The prose survives; the forty calls that produced it do not.
        assert_eq!(digest.said, ["Refactor done; three files changed."]);
        assert!(!phrase.contains("ToolCall"), "{phrase}");
    }

    /// A fourth tool name is summarised away rather than making the sentence unbounded — but
    /// the *total* stays exact, because the total is what the user is checking.
    #[test]
    fn a_long_tail_of_tool_names_is_summarised_not_dropped_from_the_total() {
        let events: Vec<_> = ["read", "edit", "bash", "grep", "write"]
            .iter()
            .enumerate()
            .map(|(index, name)| (T0 + index as u64, tool(name)))
            .collect();
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&events));

        assert_eq!(digest.activity.tools, 5);
        let phrase = digest.activity.phrase();
        assert!(phrase.contains("5 tools"), "{phrase}");
        assert!(phrase.ends_with(", …)"), "the tail was not marked: {phrase}");
    }

    /// **The correction that makes the ranking honest.** A permission asked before the user
    /// left and never answered is an agent that has been stopped the whole time they were
    /// away. A hard `at >= since` filter answers `Fine` here and reports the board as quiet.
    #[test]
    fn a_permission_asked_before_the_window_still_blocks() {
        let events = vec![(T0, asked("delete build/"))];
        let digest = AgentDigest::build(agent("Reviewer"), T0 + 10_000, feed(&events));

        assert_eq!(digest.attention, Attention::Blocked);
        assert!(digest.attention_items[0].text.contains("delete build/"));
        // The counts are still windowed: nothing *new* happened.
        assert!(digest.activity.is_quiet(), "an old event was counted as news");
    }

    /// An answered request is not a blocker, and an option set nobody picked from is —
    /// both are "the agent cannot proceed without you", which is what the level means.
    #[test]
    fn answering_clears_a_block_and_an_unpicked_option_set_creates_one() {
        let answered = vec![
            (T0, asked("write to src/main.rs")),
            (
                T0 + 1,
                TranscriptEvent::PermissionAnswer { id: RequestId("p1".into()), allowed: true },
            ),
        ];
        let digest = AgentDigest::build(agent("Reviewer"), T0, feed(&answered));
        assert_eq!(digest.attention, Attention::Fine, "{:?}", digest.attention_items);

        let offered = vec![(
            T0,
            TranscriptEvent::Options {
                prompt: "which layout?".into(),
                choices: vec![Choice::new("a", "A"), Choice::new("b", "B")],
                chosen: None,
            },
        )];
        let digest = AgentDigest::build(agent("Designer"), T0, feed(&offered));
        assert_eq!(digest.attention, Attention::Blocked);
        assert!(digest.attention_items[0].text.contains("which layout?"));

        let picked = vec![(
            T0,
            TranscriptEvent::Options {
                prompt: "which layout?".into(),
                choices: vec![Choice::new("a", "A")],
                chosen: Some("a".into()),
            },
        )];
        let digest = AgentDigest::build(agent("Designer"), T0, feed(&picked));
        assert_eq!(digest.attention, Attention::Fine);
    }

    /// A tool that answered `ok: false` is routine — an agent grepping for something that is
    /// not there. A *turn* that failed is not. Treating the first as urgent would put half
    /// the board in the attention list and make the list worthless.
    #[test]
    fn a_failed_tool_is_not_an_emergency_but_a_failed_turn_is() {
        let routine = vec![
            (T0, tool("grep")),
            (
                T0 + 1,
                TranscriptEvent::ToolResult {
                    id: ToolCallId("grep".into()),
                    output: String::new(),
                    ok: false,
                },
            ),
            (T0 + 2, ended(TurnOutcome::Completed)),
        ];
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&routine));
        assert_eq!(digest.attention, Attention::Fine);
        assert_eq!(digest.activity.tool_failures, 1);
        assert!(digest.activity.phrase().contains("1 of them failed"));

        let broken = vec![(
            T0,
            ended(TurnOutcome::Failed { message: "claude: command not found".into() }),
        )];
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&broken));
        assert_eq!(digest.attention, Attention::Failed);
        assert!(digest.attention_items[0].text.contains("command not found"));

        // The user's own stop is not a failure to report back to them.
        let stopped = vec![(T0, ended(TurnOutcome::Cancelled))];
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&stopped));
        assert_eq!(digest.attention, Attention::Fine);
    }

    /// A turn that opened and never ended is *still mid-task* — reported, ranked below a
    /// failure, and worded so it does not claim the agent is dead. This crate cannot tell.
    #[test]
    fn an_unfinished_turn_is_reported_as_mid_task() {
        let events = vec![(T0, started("port the parser")), (T0 + 1, tool("read"))];
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&events));

        assert_eq!(digest.attention, Attention::Stalled);
        assert!(digest.attention_items[0].text.contains("port the parser"));
        assert_eq!(digest.activity.turns_finished, 0, "an open turn was counted as finished");

        // Ended, and it stops being anything to report.
        let mut finished = events;
        finished.push((T0 + 2, ended(TurnOutcome::Completed)));
        let digest = AgentDigest::build(agent("Builder"), T0, feed(&finished));
        assert_eq!(digest.attention, Attention::Fine);
    }

    /// An away period with nothing in it must **say** nothing happened. An empty string
    /// renders as a blank card, which is indistinguishable from a card that failed to draw.
    #[test]
    fn an_empty_period_says_nothing_happened_rather_than_nothing() {
        let empty = Digest::new(T0, T0 + 2 * 3600 + 900);
        assert!(empty.is_quiet());
        assert!(!empty.needs_attention());

        let lines = empty.card_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Nothing happened"), "{}", lines[0]);
        assert!(lines[0].contains("2h 15m"), "the away period was not named: {}", lines[0]);
        assert!(!empty.panel_text().trim().is_empty());
        assert!(!empty.prompt().trim().is_empty());

        // An agent that was present and did nothing is the same answer, not a blank row.
        let idle: Vec<(Timestamp, TranscriptEvent)> = Vec::new();
        let mut quiet = Digest::new(T0, T0 + 60);
        quiet.add(agent("Builder"), feed(&idle));
        assert!(quiet.is_quiet());
        assert!(quiet.card_lines()[0].contains("Nothing happened"));
    }

    /// The card and the panel are two renderings of one derivation, so they cannot disagree
    /// about what happened — which is the whole reason a user would trust either.
    #[test]
    fn the_two_forms_agree_about_the_counts_and_about_the_blocker() {
        let (busy, blocked) = (busy(), blocked());
        let mut digest = Digest::new(T0, T0 + 3600);
        digest.add(agent("Builder"), feed(&busy));
        digest.add(agent("Reviewer"), feed(&blocked));

        let panel = digest.panel_text();
        let lines = digest.card_lines();
        let builder = lines
            .iter()
            .find(|line| line.starts_with("Builder"))
            .expect("the busy agent had no line at all");

        // The same tool total in both forms, from the same `Activity`.
        assert!(builder.contains("40 tools"), "{builder}");
        assert!(panel.contains("40 tools"), "{panel}");

        // And the same blocker in both, worded the same way.
        let blocker = &digest.agents[0].attention_items[0].text;
        assert!(lines[0].contains(blocker.as_str()), "{}", lines[0]);
        assert!(panel.contains(blocker.as_str()), "{panel}");

        // The prompt form carries the panel whole, so it cannot say something else either.
        assert!(digest.prompt().contains(&panel));
    }

    /// Prose is what gets shown, and it is bounded — but the panel says how much it dropped
    /// rather than pretending the agent was brief.
    #[test]
    fn prose_is_kept_most_recent_first_and_says_what_it_dropped() {
        let events: Vec<_> = (0..8)
            .map(|index| (T0 + index, said(&format!("line {index}"))))
            .collect();
        let digest = AgentDigest::build(agent("Writer"), T0, feed(&events));

        assert_eq!(digest.said.len(), MAX_PROSE);
        assert_eq!(digest.said.last().unwrap(), "line 7", "the newest line was dropped");
        assert_eq!(digest.said_dropped, 3);
        assert!(digest.panel().contains("3 lines before that"), "{}", digest.panel());
        // With no attention and no counts, the card still shows the agent's last word.
        assert!(digest.line().contains("line 7"), "{}", digest.line());
    }

    /// Every string here is agent output or a web page's own words — the least controlled
    /// data in the application — and the release profile aborts on a panic. This drives
    /// *this module's* `clip`, not just `TranscriptEvent::headline`: prose, a permission
    /// summary, a turn prompt and a tool name, all multi-byte.
    #[test]
    fn a_digest_of_multibyte_output_neither_panics_nor_runs_away() {
        let prose = "夕".repeat(400);
        let emoji = "🙂🙂🙂".repeat(60);
        let events = vec![
            (T0, TranscriptEvent::TurnStarted { turn: TurnId(9), prompt: emoji.clone() }),
            (T0 + 1, tool("読む")),
            (T0 + 2, said(&prose)),
            (T0 + 3, said("café\nsecond line")),
            (T0 + 4, asked(&emoji)),
            (T0 + 5, TranscriptEvent::Error { message: "夕".repeat(300) }),
        ];

        let mut digest = Digest::new(T0, T0 + 90);
        digest.add(agent("多言語"), feed(&events));

        for line in digest.card_lines() {
            assert!(line.chars().count() < 600, "{}", line.chars().count());
        }
        assert!(!digest.panel_text().is_empty());
        assert!(!digest.prompt().is_empty());

        let only = &digest.agents[0];
        for line in &only.said {
            assert!(line.chars().count() <= PROSE_CHARS + 1, "{}", line.chars().count());
        }
        for item in &only.attention_items {
            // The prefix is ASCII and the clipped tail is bounded; both together stay short.
            assert!(item.text.chars().count() <= ATTENTION_CHARS + 40, "{}", item.text);
        }
        // A second line below the first is marked rather than silently lost.
        assert!(only.said.iter().any(|line| line == "café…"), "{:?}", only.said);
        assert_eq!(only.activity.by_tool, vec![("読む".to_string(), 1)]);
    }

    /// Agents with the same urgency are ordered by how recently they did anything, so the
    /// board's card reads newest first — and by name after that, so two agents that stopped
    /// in the same second do not swap places between runs.
    #[test]
    fn equal_urgency_ranks_by_recency_then_by_name() {
        let early = vec![(T0, tool("read"))];
        let late = vec![(T0 + 500, tool("read"))];
        let same_a = vec![(T0 + 500, tool("edit"))];

        let mut digest = Digest::new(T0, T0 + 1000);
        digest.add(agent("Early"), feed(&early));
        digest.add(agent("Late"), feed(&late));
        digest.add(agent("Also"), feed(&same_a));

        let names: Vec<&str> = digest.agents.iter().map(|a| a.agent.name.as_str()).collect();
        assert_eq!(names, ["Also", "Late", "Early"]);
    }
}
