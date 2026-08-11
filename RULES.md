# Rules

The things you need to know before changing Velm. Not style preferences — every rule
here exists because something went wrong without it, and most of them cost real
debugging to find.

Read this before your first change. It is shorter than the code it protects.

---

## 🛑 Rule Zero — boards must survive every change

**No update, refactor, migration, rename, cleanup or fix may delete, truncate or
corrupt an existing board.** This outranks every other rule in this file, including
the performance goals.

Velm's boards live in `~/Library/Application Support/Vellum/boards/` (macOS) alongside
a content-addressed blob store. Treat that directory as **production data belonging to
someone else**. People migrate boards into Velm out of Miro, and a Miro board that has
been deleted upstream cannot be re-imported — the `.rtb` backup format is encrypted and
the REST API returns no content for a large share of items. A board lost here is very
likely lost for good.

What that forbids, in practice:

- **Never `rm` a `.vellum` file, its `-wal`/`-shm` sidecars, or the blob store** to
  "reset" or "clean up" — not in a script, not in a test, not to reproduce a bug. Use a
  scratch `HOME` or a temp directory. A board is only ever deleted by a person clicking
  Delete.
- **A schema or format change must read what is already on disk.** If a field moves,
  keep reading the old spelling; if a token changes, degrade to a default rather than
  failing the parse. `Style::locked` is the worked example — written only when true, so
  every board saved before it existed still loads byte for byte. **An unknown value
  degrades; it never aborts a load.**
- **Renaming is the sharp edge.** `.vellum` is the board extension and `BoardDb::open`
  refuses anything else, so renaming the extension orphans every board on the machine.
  This is why the `vellum-*` → `velm-*` crate rename is deliberately deferred.
- **Delete is a trash, and it must stay one.** `Library::trash` records a path in the
  sidecar and touches no file. `Library::purge` is the only thing in the application
  that removes a `.vellum`; it is reachable from *Recently deleted* alone, and it is
  confirmed. Nothing in the trash expires — no timer, no ninety days. A timer that
  deletes a board is exactly what this rule forbids.
- **Never widen a destructive path to "fix" it.** Delete honours locks; `Board::remove`
  refuses an id twice. If one of those looks like it is in the way, it is doing its job.
- **Verify against a copy, never the original.** Before anything that touches storage,
  copy a board somewhere scratch and work there.
  - ⚠ **`--board /tmp/…` is not isolation.** The *board* goes to `/tmp`, but the app
    still opens the real `boards/library.json`, and `set_last_board` **writes the
    scratch path into it** — so the next real launch tries to open a board that has been
    deleted. This has happened. Set `HOME` to a scratch directory for anything that
    opens a window, and put the board under it:

    ```bash
    HOME=$(mktemp -d) ./target/release/vellum-app --board "$HOME/x.vellum" …
    ```
- **Reset a scratch board with `rm -f x.vellum*`, not `rm x.vellum`** — SQLite is in WAL
  mode and the `-wal` sidecar replays the items back. The glob matters.

If a change genuinely cannot preserve existing boards, **stop and say so** rather than
shipping it with a migration nobody asked for.

---

## ✍️ Attribution

**No AI tool is ever listed as an author, co-author, or contributor.**

- No `Co-Authored-By:` trailer naming an assistant, model, or tool.
- No "generated with", "written by", or bot attribution in commit messages, pull
  request descriptions, changelogs, or source headers.
- Commits are authored by the person who is accountable for them.

Use whatever tools you like to write the code. The commit log records people.

---

## Decisions already made — do not re-litigate

Each of these was argued once and settled. Reopening one costs a review cycle and
lands back in the same place.

| Decision | Why |
|---|---|
| **Pure native. No webview.** | Tauri closed WebGPU flag support; WKWebView was frame-capped at 60fps until macOS 26. See `docs/01-architecture.md` §1. |
| **`egui-wgpu` is not used** | It pins wgpu 29 while the project is on wgpu 30. `vellum-render` draws egui itself. §6a. |
| **Collaboration is cut entirely** | No presence, cursors, comments, voting, or sync. This is a single-player tool on purpose. |
| **Loro is the document layer** | For undo/redo and history, not for collaboration. |
| **Light mode only** | The dark tokens stay in `vellum-ui::theme` — unreachable, tested, and cheap to bring back. |
| **Mouse first; trackpad tuning deferred** | Mouse wheel zooms (`LineDelta`); trackpad two-finger pans (`PixelDelta`). |
| **No drag inertia** | A mouse has no momentum. |

---

## Traps that already cost real debugging

Every one of these was found the expensive way.

1. **Miro coordinates are mostly parent-relative.** Offsets are measured from the
   parent's **top-left** and scale by the **parent's** scale:
   `absolute = parent_abs + offset × parent_scale − parent_size × parent_scale / 2`.
   Getting this wrong misplaced 39 ink strokes by up to 641px while looking plausible.

2. **Clipboard payloads are delimited, not prefixed:**
   `<--(miro-data-v1)…(/miro-data-v1)-->`. Feeding the closing marker into base64
   corrupts it. Decode strictly — leniency hides the bug.

3. **`.rtb` board content is encrypted and unrecoverable.** Proven, twice.
   `canvas.json` measures entropy 7.9987/8, deflates to 0% inside the ZIP, and has
   10,346 of 10,346 distinct 16-byte blocks. Lengths fit a 12-byte nonce + 16-byte AEAD
   tag; `meta.json` says `"encryptionVersion":"1.1"` outright. The key is not on the
   client — restore is a server-side upload. Don't retry it. The `.rtb` is an **asset**
   source (images, documents, preview pictures), which is what it is used for.

4. **Screen coordinates are physical pixels everywhere.** Mixing logical and physical
   makes zoom-at-cursor drift by exactly the scale factor.

5. **A left press is resolved by the application, not by `input.rs`.** Whether a drag
   moves the selection or sweeps a marquee depends on what is under the pointer, and
   that module deliberately knows nothing about the scene. A press returns
   `Intent::Press` and `crate::actions` must answer `Input::resolve_press` *before the
   next event*. Forgetting falls back to a marquee.

6. **egui matches modifiers loosely** — `⌘Z` swallows `⌘⇧Z` unless bindings are consumed
   most-specific-first.

7. **`loro`'s `tree.nodes()` includes deleted nodes.** Liveness needs `is_node_deleted`.

8. **`rusqlite` is held at 0.39.** 0.40 needs a `libsqlite3-sys` whose build script uses
   unstable `cfg_select!`.

9. **`egui-winit` eats `⌘V`, `⌘C` and `⌘X` — they never become `Event::Key`.** Its
   `on_keyboard_input` matches the three clipboard chords first, pushes
   `Event::Paste`/`Copy`/`Cut`, and **returns before the key is pushed**. So a shortcut
   table can never match them. Worse, the `Paste` event is built from `arboard::get_text`
   and the `return` sits *outside* that `if let` — with an image-only pasteboard, which
   is exactly what a screenshot leaves, **no event of any kind is produced**.
   `Shell::restore_clipboard_key` puts the key back after delegating, additively.
   **The general trap: any binding `egui-winit` translates before egui sees it is
   invisible to a shortcut table, and a test that synthesises `egui::Event::Key` directly
   starts downstream of the drop and passes anyway.**

10. **cosmic-text does not fall back to a family's regular face when the requested
    weight is missing — it leaves the family.** A bold span was being set in Courier for
    months. `TextEngine::family_has_bold` drops the weight rather than the family.
    Inter is bundled now, and note what the fix actually was: loading the faces changed
    nothing on its own — **`set_sans_serif_family` is what points the alias at them**.
    **The lesson is about the test:** the old assertion was `bold.width >= regular.width`,
    which a monospace fallback satisfies comfortably, so it was green throughout.
    **A width assertion cannot tell "heavier" from "different."**

11. **An undo group that leaks breaks the board for the rest of the session.** Loro's
    `UndoManager::group_start` answers `Err(UndoGroupAlreadyStarted)` when a group is
    already open, and `group_end` merely clears the slot — there is no depth count. So a
    single `?` escaping between begin and end leaves the group open forever, and **every**
    later grouped operation fails. `Editor::edit` is the chokepoint: it closes the group
    **and reprojects** on the error path. **A `Result` in a paired begin/end block is an
    unwind path, and Rust's `?` does not run the `end`.**
    Three parts to the rule, because the chokepoint alone was not enough:
    - Open a group only inside `Editor::edit`.
    - Close a deliberately long-lived group (the on-canvas caret and the eraser sweep
      both hold one open across many calls, on purpose) before dispatching any
      board-mutating command — `Command::mutates_board`.
    - Give every way a gesture can *end without a release* — Escape, a tab switch,
      changing tool, quitting — a call to the function that closes it.

12. **`[profile.release]` sets `panic = "abort"`.** A panic in a painter closure does not
    unwind, it kills the app — on the frame an item becomes visible, and again on
    relaunch, because a board reopens at the same camera. Two string-slicing bugs did
    exactly this. Prefer `str::get` over `&s[a..b]` on anything derived from board
    content: it is panic-free *by construction* rather than by argument.

13. **A fix at the door does nothing for what is already in the room.** Decoding HTML
    entities at import and at fetch left every board already on disk showing `&#39;`.
    The repair had to happen at *paint*, which fixes existing boards with no migration
    and no write — which matters more than a single decode point, because Rule Zero's
    posture is that a board is someone else's production data.

---

## Verification

```bash
cargo test --workspace                    # 2,417 tests across the workspace
cargo clippy --workspace --all-targets    # must be zero warnings
```

Two things about the suite that will otherwise waste your afternoon:

**`vellum-render`'s `glass_budget.rs` is load-sensitive and will fail spuriously.** Its
assertions are GPU wall-clock against a 500µs budget, and `cargo test` runs test binaries
in parallel, so the suite competes with itself for the one GPU. Measured on an 8-core
laptop: 1109µs (fail) under load, passing on the same commit when quiet. **A failure here
is a scheduling artefact until reproduced on an idle machine** — re-run it alone with
`cargo test -p vellum-render --test glass_budget` before believing it, and do not "fix" it
by raising the budget.

**The reference-board tests skip cleanly when the exports are absent**, which is the case
on every fresh clone and in CI. They look for a `.rtb` and an `.html` capture by
*extension*, at the repository root and in `captures/` respectively — so whichever board
you hold will do. A skip prints the reason; it is never mistaken for a pass.

### Diagnostics — how a claim gets measured

A dialog, a paste and a live gesture cannot be photographed by an unattended run, so the
app carries its own harness. These exist because **a diagnostic that skips the input layer
cannot verify a feature people reach through the input layer** — a lesson that cost three
rounds of "paste is fixed".

| Flag | What it is for |
|---|---|
| `--screenshot PATH` | One composited frame — board, glass and chrome — rendered **offscreen** and written to PNG. A window behind another window is never presented to (`get_current_texture` answers `Occluded`), so this is the only honest check of the chrome. |
| `--demo NAME` | Builds a fixture board, or **drives a real gesture** through the production input path and reports a verdict. `--demo snapping`, `--demo context-menu`, `--demo widget-edit`, `--demo edit-then-delete`, … |
| `--show NAME` | Raises chrome only a click can otherwise reach — `properties`, `palette`, `find`, the flyouts, `menu`. Repeatable; they compose. |
| `--open-dialog NAME` | Raises a modal on the first frame. |
| `--hud`, `--exit-after SECS` | Frame timings, and a run that terminates unattended with an average frame rate. |

`./target/release/vellum-app --help` is the full list.

---

## Build discipline

- **One `cargo` invocation at a time.** Two concurrent builds on a small machine is how
  this project produced two kernel panics. `.cargo/config.toml` caps `jobs = 4` for the
  same reason — raise it on a bigger machine, but watch memory pressure while a
  `--profile dist` build runs.
- **Run the built binary, not `cargo run`**, when you are driving the app — otherwise a
  concurrent build holds the cargo lock.
- **`target/` gets very large.** `cargo test --workspace` builds ~65 test binaries and
  `cargo clippy --all-targets` keeps its own artifact set beside them; together they have
  reached 21 GB. Nothing in it is precious. `cargo clean`, or `rm -rf target/debug`
  mid-session to free the bulk while keeping the release binary the `--demo` fixtures need.

### Profiles

| Profile | Use |
|---|---|
| `--profile quick` | The edit loop. No LTO, opt-level 2. |
| `--release` | Thin LTO, 16 codegen units. What the diagnostics run against. |
| `--profile dist` | Fat LTO, one codegen unit. **The shipping build. Run it alone.** |

---

## Working style

- **Verify claims by running things.** Report measured numbers, never intentions.
- **Name gaps honestly.** A known gap is fine; a false claim is not. Every unimplemented
  path in the app answers with a toast naming what is missing — nothing is inert, and
  nothing pretends to work.
- **A count that is displayed but never asserted is a claim nobody checks**, and it is
  more dangerous than an untested function because it reads like evidence. One such
  number survived long enough to become a documented defect that did not exist.
- **A test written by the author of a change tests what the author was already thinking
  about.** Two adversarial reviews of already-green, already-shipped work found six real
  defects each. If a test passed before your fix and after it, it is not testing your fix
  — A/B it against the unfixed build.
