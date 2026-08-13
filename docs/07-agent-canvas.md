# 07 — The Agent Canvas layer

Velm's boards become a live multi-agent workspace: frames that run real AI agents, connector
lines that carry messages between them, notes that are real `.md` files on disk, and an
orchestrator that owns a region of the board.

This document is the **contract**. Every module below is built against it, and where two
pieces meet, the rule that governs the join is stated here rather than in either one.

---

## 0. The three rules that outrank everything in this document

1. **RULE ZERO holds.** No agent feature may delete, truncate or corrupt a board. Agent
   output never enters the Loro document except as configuration the user chose. Transcripts
   live in a disposable sidecar (§4) precisely so that losing one costs history and never
   content.
2. **No idle cost.** A board with no agent nodes must be byte-for-byte the same file, and
   frame-for-frame the same cost, as before this layer existed. Nothing starts a thread, a
   process, a socket or a timer until the user places an agent node.
3. **Everything expensive is opt-in.** Browser nodes, worktrees, scheduling, voice, telemetry:
   off by default, degrading to a legible explanation rather than a dead button.

---

## 1. What is added, and where

```
crates/
  vellum-agent/          NEW. All agent logic that does not need a window or a GPU.
  vellum-app/
    agent.rs             NEW. ItemKind::Agent ⇄ AgentModel, and the measurement
                              vellum-agent deliberately does not do.
    agent_runtime.rs     NEW. The live session pool: threads, channels, drained once
                              per frame. The `links.rs` shape.
    agent_draw.rs        NEW. Painting an agent node's transcript, status and controls.
    filetree.rs          NEW. ItemKind::FileTree token + measurement.
    note.rs              NEW. ItemKind::AgentNote token + the on-disk `.md` mirror.
    browser.rs           NEW. ItemKind::Browser token + the wry overlay (feature-gated).
    voice.rs             NEW. Push-to-talk capture and transcription hand-off.
  vellum-ui/
    agent_panel.rs       NEW. Inspector rows for an agent node.
    agent_dialogs.rs     NEW. Schedule editor, rules editor, provider sign-in.
```

`vellum-agent` depends on `serde`, `serde_json`, `thiserror`, `anyhow`, `ureq`,
`portable-pty`, and **nothing from the workspace**. It compiles and tests on a machine with
no graphics stack, exactly as `vellum-doc` and `vellum-flow` do. The dependency arrows keep
pointing into `vellum-app`.

### Module map of `vellum-agent`

| module | what it owns |
|---|---|
| `model.rs` | `AgentModel` — the opaque token stored in the document. Config only. |
| `provider.rs` | `Provider`, `ProviderConfig`, credential resolution, per-node assignment |
| `transport/acp.rs` | Agent Client Protocol client: JSON-RPC 2.0 over a child's stdio |
| `transport/pty.rs` | PTY host for CLI agents, via `portable-pty` |
| `transport/http.rs` | Direct API: Claude, OpenAI, Kimi, any OpenAI-compatible local server |
| `transcript.rs` | `TranscriptEvent` — the one stream every transport produces |
| `session.rs` | The running-agent state machine: idle → running → error, turn lifecycle |
| `rules.rs` | The three-layer cascade (§7) |
| `notes.rs` | File-backed markdown notes: dual scope, linking, external-change detection |
| `filetree.rs` | Scoped, lazy directory reads |
| `worktree.rs` | `git worktree` lifecycle, opt-in |
| `bus.rs` | Agent-to-agent messages, and the loopback IPC server the CLI shim talks to |
| `mcp.rs` | Velm's own MCP server surface |
| `research.rs` | The web-research tool (§12) |
| `schedule.rs` | Schedules, triggers, completion actions |
| `orchestrator.rs` | Territory and spawn caps |
| `ingest.rs` | Any-file-type context ingestion |
| `summary.rs` | Away-mode summaries |

Two new binaries, both tiny:

- `velm-agent-cli` — the shim an agent process invokes to talk to Velm (send a message to a
  connected agent, read/write a note, spawn a sub-agent, post an image or an option set).
- `velm-mcp` — Velm's MCP stdio server, so any MCP-speaking agent gets the same surface.

---

## 2. Document schema — four new `ItemKind` variants

Following the precedent of `Table`, `Chart`, `MindMap` and `Kanban` exactly: **one opaque
string token per variant**, encoded by `vellum-app`, stored verbatim by `vellum-doc`.
`vellum-doc` depends on `loro` and `thiserror` and nothing else, and that does not change.

| variant | tag | token |
|---|---|---|
| `Agent { model: String }` | `"agent"` | serialised `vellum_agent::AgentModel` |
| `FileTree { model: String }` | `"file_tree"` | serialised `FileTreeModel` |
| `AgentNote { model: String }` | `"agent_note"` | serialised `NoteModel` |
| `Browser { model: String }` | `"browser"` | serialised `BrowserModel` |

**Why four variants and not one with a discriminator.** A file tree, a note and a browser
are not agents; they lay out differently, hit-test differently and are created by different
tools. One variant would put a `match` on a JSON field in the painter's hot path and make
`ItemKind::tag()` lie in the import-fidelity report that joins on it.

**Worker, orchestrator and meta agents are all `ItemKind::Agent`**, distinguished by
`AgentModel::role_kind`. They differ in configuration and in which tools they may call, not
in what they are on the canvas.

### RULE ZERO consequences, and the one that is not free

- A board with no agent nodes writes no new keys. Existing boards are untouched, byte for byte.
- Reading is additive: a build with this layer reads every board written without it.
- **`read_kind` is strict** — it returns `DocError::Malformed` for an unknown tag, and
  `Board::items` propagates that. So a board containing agent nodes **cannot be opened by a
  build that predates this layer.** That is a forward-compatibility break, not a data loss:
  the file is intact and the current build reads it. It is called out here because it is the
  one place this layer is not invisible to older code, and because the mitigation — making
  `read_kind` lenient — would put invisible items on the canvas and is worse.
- Agent *output* is never written to the document. See §4.

### `ItemKind::text()`

An agent node's **role label** and a note's **title** answer `text()`, so both are searchable
and editable through the paths that already exist. A transcript does not: it is not text the
user wrote, which is the same rule that keeps a link card's scraped title out of search.

---

## 3. Agent connectors — derived, not stored

**A connector is an agent link when both of its endpoints are bound to agent-family nodes.**
Nothing is added to `ItemKind::Connector`.

Rationale: a stored flag is a second source of truth that can disagree with the endpoints,
and this repo has already paid for that twice (the grid's two controls, `locked: false`).
Derivation cannot drift, needs no migration, and makes an agent link out of every connector
the user has already drawn between two agents.

- **Direction is the arrowhead.** One end with an arrowhead → messages flow that way. Both or
  neither → bidirectional. This reuses a control the user already has on the context bar.
- **Visual distinction**: an agent link draws with a distinct dash cadence and, while a
  message is in flight, an animated pulse travelling along the routed path. The pulse repaints
  only while a message is actually passing and stops — the `landed_background` rule from
  feedback 21: no timer driving redraws while nothing is happening.
- A connector between an agent and a **note** or a **file tree** is a *context* link: the
  agent reads that note or that subtree as context. Same derivation, different line colour.

---

## 4. Transcripts live outside the document

```
~/Library/Application Support/Vellum/agents/<board-key>/<item-id>.jsonl
```

`<board-key>` is a hash of the board file's canonical path, so a transcript follows its board
and two boards never collide.

**FNV-1a, not BLAKE3** — an earlier draft of this document said BLAKE3 and the code is right
to disagree. This key names a cache directory: it is not a content address, nothing verifies
it, and an attacker who could choose a colliding board path already has the filesystem. Using
BLAKE3 would mean `vellum-agent` taking a dependency purely to name a folder, and this crate's
whole discipline is that it depends on nothing in the workspace.

The filename within it is the item id, put through a sanitiser that makes `..` **unwritable
rather than checked for** — the escape is refused by construction, and two different ids can
never sanitise to the same file, which would silently overwrite one agent's transcript with
another's.

- **Append-only JSONL of `TranscriptEvent`.** Cheap to append, cheap to tail, survives a crash
  mid-write (a torn last line is dropped on read).
- **Disposable.** Deleting the whole directory loses history and no board content. The app
  treats a missing transcript as an agent that has not run yet.
- **Never in Loro.** Streaming agent output into a CRDT would make every token an undo step,
  bloat the board file without bound, and put third-party text inside the file RULE ZERO
  protects.
- Images an agent emits go in the **existing BLAKE3 blob store**, addressed exactly like a
  pasted screenshot, and the transcript event carries the hash. Deduplication and the 268MB
  residency budget come for free.

---

## 5. Execution — three transports, one event stream

Every transport produces the same `TranscriptEvent` stream, so the painter and the display
modes know nothing about which one is running.

```rust
enum TranscriptEvent {
    TurnStarted { turn: TurnId, prompt: String },
    Thought { text: String },                        // raw mode only
    ToolCall { name: String, input: String, id: ToolCallId },   // raw mode only
    ToolResult { id: ToolCallId, output: String, ok: bool },    // raw mode only
    Text { text: String },                           // both modes
    Image { blob: String, caption: Option<String> },  // both modes
    Options { prompt: String, choices: Vec<Choice> }, // both modes — §11
    PermissionRequest { id: RequestId, summary: String, detail: String },
    Message { from: AgentRef, text: String },        // arrived over a connector
    TurnEnded { turn: TurnId, outcome: TurnOutcome },
    Error { message: String },
}
```

### 5a. ACP — the default, and the answer to bring-your-own-subscription

Agent Client Protocol: JSON-RPC 2.0 over the child process's stdin/stdout. Velm is the
*client*; it initialises a session, sends prompts, and receives streamed updates, permission
requests and tool notifications back.

This is what makes **feature 17** work. `claude`, `codex` and `gemini` are already installed
and already hold the user's subscription credentials. Velm delegates execution to that
process and never sees a token or a bill. No API key is required for these providers.

### 5b. PTY — for a real terminal, and for agents with no ACP mode

`portable-pty` spawns the CLI in a pseudo-terminal with its own working directory. Output is
parsed for the subset of ANSI that matters (SGR colour, erase-line, cursor-home) and rendered
through Velm's own text pipeline. This is Maestri's mechanic, and it is what lets one agent
literally type into another agent's terminal (§6).

### 5c. HTTP — direct API, and local models

Claude, OpenAI, Kimi, and **any OpenAI-compatible endpoint**, which is how a model on the
user's own GPU (llama.cpp, LM Studio, Ollama, vLLM) is supported without a fourth code path.
Blocking `ureq` on a worker thread, posting events back through a channel — the shape
`links.rs` already uses. No `tokio`, no second execution model.

### 5d. Threading and idle cost

One thread per **running** session. A session with no turn in flight is either blocked on a
read (ACP/PTY — the OS wakes it, we spend nothing) or has no thread at all (HTTP). The pool is
drained once per frame in `agent_runtime.rs`, exactly as `links.rs` drains fetches.

**Viewport culling keeps processes alive and stops rendering them** — Maestri's rule, and the
one that makes dozens of agents affordable. An off-screen agent node is not shaped, not
tessellated and not uploaded; its process keeps running and its transcript keeps appending.

---

## 6. Agent-to-agent messaging, and the loopback server

Two halves, because the *user's* agents run in their own processes and cannot call into Velm.

- **In-process bus** (`bus.rs`) routes a message from one session to another along a derived
  agent connector (§3), applying the direction rule and refusing a message with no link.
- **Loopback IPC server** binds `127.0.0.1:0` — an ephemeral port, loopback only, never a
  wildcard bind. The port and a per-launch random token are written to
  `~/Library/Application Support/Vellum/runtime/ipc.json`, mode `0600`. Every request carries
  the token and the calling agent's node id.

`velm-agent-cli` is the shim: agents are launched with `VELM_IPC` and `VELM_AGENT_ID` in
their environment, and the shim on `PATH`. `velm-agent-cli send <target> <text>` is how one
agent talks to another; the same shim carries note reads/writes, sub-agent spawning, image
posting and option sets. Agents that speak MCP get the identical surface through `velm-mcp`.

**Loops are bounded.** A message carries a hop count; the bus refuses beyond a configurable
depth (default 8) and reports it on the receiving node rather than silently dropping it.

---

## 7. The rule cascade

Three layers, each inheriting from the one above unless it overrides:

| layer | stored in | applies to |
|---|---|---|
| Global | `<data-dir>/rules/global.md` | every agent, every board |
| Project | `<board dir>/.velm/rules.md` | every agent on that board |
| Agent | the node's `AgentModel` | that node alone |

- Rules are **markdown with a small front-matter block**, so they are readable and editable in
  any editor, and so an existing `CLAUDE.md`/`AGENTS.md`/`.cursorrules` can be pointed at
  directly rather than copied. Project discovery order is `.velm/rules.md` → `AGENTS.md` →
  `CLAUDE.md` → `.cursorrules`, **first hit wins rather than merged**: a project with two of
  these has two opinions, and silently concatenating them produces a rule set neither file's
  author wrote.
- **The structured settings live in the front matter and nowhere else.** An earlier draft of
  this document paired `global.md` with a `rules.json`; that was wrong and is withdrawn. Two
  files describing the same four settings is a second source of truth for hand-edited values,
  which is the exact failure this file keeps recording — one of the pair inevitably becomes
  the one nothing reads.
- An unrecognised value for a structured setting **falls through to the layer above**, not to
  a default. That matters most for the permission posture: a permission granted by a typo is
  the one failure this cascade cannot be allowed to have.
- **All three layers are writable.** `RuleFile::to_markdown` is the exact inverse of
  `RuleFile::parse` — unknown front-matter keys included, since the parser keeps them
  precisely so that a save does not drop them — and `RuleFile::save` writes atomically,
  creates the file and its directories when there is none (the common case: a global rule set
  starts by being written), and **refuses** rather than overwriting a file that changed since
  the editor was opened. Velm writes the project layer to `.velm/rules.md` and never to
  somebody else's `AGENTS.md`/`CLAUDE.md`/`.cursorrules`, which it only ever reads.
- `rules::resolve(global, project, agent)` returns a `ResolvedRules` that records, per field,
  **which layer supplied it**. The inspector shows *inherited* versus *set here* from that
  record rather than by re-deriving it, so the display cannot disagree with what the agent got.
- The role label (§ feature 5) is folded into the resolved system context at the agent layer.

---

## 8. Notes are real files

A note node stores a **path**, a scope and its links; the content lives in a `.md` file on
disk and nowhere else.

- **Shared scope**: `<project>/.velm/notes/<slug>.md` — every agent on the board may read and
  write it.
- **Private scope**: `<project>/.velm/notes/<agent-slug>/<slug>.md` — one agent only. The
  restriction is enforced at the IPC boundary, not by file permissions, because the user must
  keep full access.
- **External edits win on a clean node.** The file's mtime and size are checked when the node
  becomes visible and on a low-frequency poll while it is; a changed file reloads. A note being
  edited on the canvas holds the buffer until the caret leaves, then writes. A conflict —
  changed on both sides — keeps both, writing `<slug>.velm-conflict.md`, and says so on the node.
- **Note-to-note links** are ordinary markdown links to sibling notes. An agent asked to follow
  a chain gets the transitive closure, depth-bounded — and cycle-safe, because two notes
  pointing at each other is the first thing anyone builds.
- Writes are debounced and atomic (write temp, rename), so an agent reading mid-write never
  sees half a file.
- **A board with no project directory still gets notes**, at
  `<data-dir>/agents/<board-key>/notes/`. A note must always have somewhere to live: the
  alternative is a note node that cannot be created on a board that is not a code project,
  which is most boards.

### 8a. Credentials

API keys live in `<data-dir>/credentials.json`, created mode `0600`, and **nowhere else**.
Never in a board file, never in a transcript, never in an error message, never in a log line.
The repository is public and a key pasted into a fixture is a key that is published; this is
stated here so that no module has to decide it for itself.

Most agents need no key at all — that is what §5a is for. A key is required only for
[`Transport::Http`] against a hosted provider, and never for a local model.

---

## 9. Worktrees, territory and caps

- **Worktrees** (`worktree.rs`) are **off by default**, one toggle per project. On, each coding
  agent gets `git worktree add <state-dir>/wt/<node-id> -b velm/<node-id>`. Removal is
  explicit and confirmed; an agent's worktree is never force-removed with uncommitted changes.
- **Territory**: an orchestrator holds a world-space rectangle. It may only spawn inside it,
  and the region is drawn as a labelled tint while the orchestrator is selected.
  - **The gesture is armed, then swept** — the verb is asked for (`ActiveState::arm_territory`)
    and the *next* drag on the board answers it. Deliberately not a bare drag: a drag on empty
    board is a marquee, which is the gesture that gets used constantly, and stealing it
    whenever a manager happened to be selected would take marquee selection away from every
    board with an orchestrator on it — silently, since the two look identical until the button
    comes up. Escape abandons, and abandoning leaves the previous region in force; a *click*
    abandons too, because there is no defensible default region a tap could have meant.
  - The tint is drawn in the **screen** view — fill, dashed edge and label together. A
    world-unit dash is a solid line when zoomed out and three dashes across the window when
    zoomed in, and a world-sized label is illegible at a fitted 4%; drawing all three in one
    view is also what keeps the fill and the edge from disagreeing by a rounding.
- **Spawn cap**: a hard integer. `orchestrator.rs` refuses the spawn that would exceed it and
  reports the refusal into the transcript, so the orchestrator can adapt rather than silently
  failing. Cap and territory are both enforced in Velm, never in the prompt — a limit that
  lives only in an instruction is not a limit.

---

## 10. Scheduling

Full configuration UI and local execution in phase 1: a schedule (interval / daily / weekly /
cron expression), optional trigger conditions, and a completion action — *report to the user*,
*hand off to a connected agent*, or *do nothing*.

The scheduler is a single thread holding a sorted queue of next-fire times; it wakes for the
next one, not on a poll. With no schedules configured it does not exist.

**Explicitly deferred to a later phase**: firing while the app is not running. That needs a
process outside Velm — a launch agent, or a server — and the user's laptop is closed when it
would matter. The configuration written now is what that phase would read; nothing about the
schema changes.

---

## 11. Visual output and selectable options

`TranscriptEvent::Options` carries a prompt and a list of choices, each with optional image,
title and body. The painter lays them out as a row of cards inside the node; clicking one
sends the selection back to the agent as the next turn's input and marks the chosen card.

This is the pattern the Claude Code VS Code extension cannot do — images inline at all — and
it is nearly free here, because the blob store, the texture cache and the card layout all
already exist.

---

## 12. Web research

An MCP tool (`velm-mcp`) exposing `research.search` and `research.fetch`, built on a real
browser session rather than a bare HTTP GET, because a bare GET is what gets refused.

**What this is:** a normal browsing client — a real engine when browser nodes are enabled,
otherwise a well-behaved HTTP client with a realistic user agent, a cookie jar, HTTP/2,
connection reuse, sane request pacing, and content extracted from the rendered page.
It **honours `robots.txt`, rate limits itself per host, and identifies itself honestly.**

**What this is not:** CAPTCHA solving, IP rotation, fingerprint spoofing, or anything whose
purpose is to defeat a site's access controls. A refusal is reported to the agent as a refusal.

---

## 13. Verification standard

Nothing here is done because it compiles. Per the repo's culture:

- **Pure logic** — unit tests in the owning module.
- **Anything reached through the input layer** — a `--demo` fixture driving real `winit`
  events through `Input` → `act_on`, reading the answer back out of the document. A test that
  calls the handler directly enters below the thing being tested.
- **Anything on screen** — `--screenshot`, measured in pixels where a count would pass either way.
- **Anything that could pass on the unfixed build** — A/B it, and record the measured
  before/after.
- Every gap answers with a toast naming what is missing. Nothing is inert.


---

## 14. What is actually reachable

A contract that describes what was designed, with no record of what a user can reach, is how
nine separate features come to be written, tested and callerless in one codebase. This section
is the honest ledger, from a reachability audit that traced each feature from the gesture to
the effect and verified every hop had a caller by grep.

**The distinction that matters:** *implemented* means the code exists and its tests pass.
*Reachable* means a person can get to it. Nearly everything here was implemented long before it
was reachable, and the gap between the two was invisible to a green suite.

| Feature | Gesture | State |
|---|---|---|
| 1 Live agent nodes | Palette ▸ Agent (or `A`), drag; Run on the node | Reachable |
| 2 Raw / clean per node | The node's mode toggle; the bar; Edit ▸ Agent; Preferences for the default | Reachable |
| 3 Agent-to-agent messaging | Connector between two agents; arrowhead sets direction | Delivery reachable; an agent *initiating* one needs the shim |
| 4 Worktree isolation | Preferences ▸ Worktree isolation, then run a coding agent | Reachable — created at launch, recorded, used as cwd. Removal is deliberately manual |
| 5 Custom role labels | Double-click the role, or the panel | Reachable, and folded into the system context |
| 6 Meta agent | Palette ▸ Agent ▸ Meta | Placeable; its configure power runs over the shim |
| 7 File tree nodes | More ▸ File tree | Reachable; per-agent scoping needs its editor |
| 8 Note nodes | More ▸ Note | Reachable once a note is given a file |
| 9 Orchestrator territory + cap | Palette ▸ Agent ▸ Orchestrator | Cap enforced; spawn runs over the shim; the territory drag is not built |
| 10 Scheduled agents | Edit ▸ Agent ▸ Schedule… | Reachable, end to end |
| 11 Hierarchical rules | Edit ▸ Agent ▸ Rules… | Agent layer editable; the two above are shown and revealable |
| 12 Voice | Push-to-talk on the node | Needs `--features voice`; a default build says so |
| 13 Browser nodes | More ▸ Browser + Preferences + Load | Card reachable; live pages need `--features browser` |
| 14 Images and options | An agent emits them | Images reachable; option sets arrive over the shim |
| 15 Away summaries | Leave the window, come back | Reachable, as a headline |
| 16 Provider per node | The context bar, or the panel | Reachable; a local model needs its endpoint field |
| 17 Bring-your-own-subscription | Provider ▸ Claude | **Reachable and measured** — no API key |
| 18 File-type ingestion | Drop a file on an agent, or attach from the panel | Needs its producer wired |
| 19 Web research | An agent calls the MCP tool | Runs over the shim and its MCP registration |

### The chain six of these share

Messaging-initiation, the meta agent's configure, orchestrator spawn, option sets, an agent's
note access and web research **all** terminate at `velm-agent-cli` / `velm-mcp`. That chain had
three independent breaks, any one fatal: the binaries were not built or shipped; nothing put
them on a launched agent's `PATH` or registered the MCP server; and the system context never
mentioned they existed. All three are closed — but note what is still not proven: **a real
agent process invoking the shim and Velm's handler answering has never been driven end to
end.** Every layer is tested and one hop is not. That is trap 9's exact shape, and a
*"messaging does nothing"* report should start there rather than in the bus.

### The rule this section exists to enforce

**Never describe a gesture the user cannot perform.** Three strings shipped in this layer
telling the user to drop a file on a node, drag a region on the board, and connect a note to an
agent — none of which existed. A disabled control with a tooltip naming what is missing is the
house style; an enabled-looking instruction for a gesture that does not exist is worse than
silence, because it costs the user their time before it costs them their trust.
