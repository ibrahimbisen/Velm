# The Agent Canvas, archived

Moved out of the running app on 2026-08-22 at the user's request — *"i want you to move it and
seperate it … dont delete anything i jsut want it to be archived for now i can make a better
vesion of later i just dont want it to be part of the app"*.

**Nothing here was deleted.** Every file arrived by `git mv` and keeps its full history. The
workspace is `members = ["crates/*"]`, so moving the crate out of `crates/` is what took it out
of the build — there is no feature flag and no `#[cfg]` doing it.

| Path | What it was |
|---|---|
| `vellum-agent/` | the whole crate: providers, three transports, sessions, the transcript sidecar, the rule cascade, worktrees, ingestion, the inter-agent bus, the loopback IPC server, an MCP server, web research, voice — and the `velm-agent-cli` and `velm-mcp` binaries |
| `vellum-app-agent/` | the app's half: `agent`, `agent_runtime`, `agent_view`, `note`, `filetree`, `browser`, `voice`, `browser_engine` |
| `vellum-ui-agent/` | the chrome's half: `agent_panel`, `agent_dialogs` |
| `vellum-ui-agent/tests/interaction.rs.before-archive` | the interaction test file **as it stood before the archive**, whole. 532 lines and 16 functions were taken out of the live file; this is the copy that keeps them |
| `docs/07-agent-canvas.md` | the contract — read this first if you bring the layer back |

`velm-agent-canvas-orchestrator-prompt.md` **is** here now. It sat at the repository root
until the user asked for the root to be cleared, and it belongs with the layer it drives:
the prompt plans a feature that is archived, so a reader who finds one should find the other.
Its contents are untouched, which was the point of keeping it in the first place.

## 🛑 What deliberately did NOT move, and must not

`vellum-doc` keeps `ItemKind::{Agent, FileTree, AgentNote, Browser}`, byte for byte.

RULE ZERO: archiving the *feature* must not archive the *format*. A board that ever held an
agent node has to still load, still round-trip and still save. The user's 45 boards contain
none — measured, all of them — and that is luck rather than licence. The app draws those four
kinds as a plain labelled rectangle now, which is what `push_scene_item` already does for any
kind the painter has no special drawing for.

For the same reason `vellum-app/src/library.rs` still *parses* `agent_display`,
`agent_provider`, `agent_chat_theme`, `browser_nodes`, `worktrees` and `speech` from
`library.json` and simply never reads them. Those keys are in the user's real sidecar; deleting
the fields would make the file fail to parse and take their spaces, stars and trash with it.
Feedback 31 left the `grid` key behind for exactly this reason.

## Bringing it back

    git mv archive/vellum-agent crates/vellum-agent

then restore the dependency lines in `Cargo.toml`, `crates/vellum-app/Cargo.toml`,
`crates/vellum-ui/Cargo.toml` and `crates/vellum-project/Cargo.toml`, and put the modules back.
`git log --follow` on any file here shows every change it ever had.

The honest note for whoever does: CLAUDE.md feedback 34–37 records what this layer cost the
first time — six agents wrote ~20k lines in parallel with no compiler, and two adversarial
reviews plus a reachability audit found more defects than the build did. Eight of nineteen
features were unreachable when first declared done. Read those entries before starting.
