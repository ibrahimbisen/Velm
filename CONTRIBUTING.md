# Contributing to Velm

Thanks for wanting to help. This is a small project with strong opinions, and both of
those are good news for a contributor: the code is consistent, and it is easy to find
out whether an idea will land before you write it.

**Read [RULES.md](RULES.md) first.** It is not boilerplate. It has the one rule that
outranks everything (boards must survive every change), the decisions that are settled,
and thirteen traps that each cost real debugging to find. Ten minutes there will save
you an afternoon.

---

## Before you write code

**Open an issue first for anything non-trivial.** Not bureaucracy — Velm has a
[feature catalogue](docs/features/README.md) of roughly 120 rows, and a handful of
things that look missing are missing *on purpose*. Real-time collaboration is cut
entirely. There is no dark mode. Link cards are cards, not live embeds. A PR that
implements one of those is work nobody can merge.

Small fixes — a typo, a panic, an off-by-one — just send them.

## Setting up

You need a Rust toolchain (edition 2024) and, on macOS, the Xcode command line tools.

```bash
git clone https://github.com/ibrahimbisen/Velm.git
cd Velm
cargo build --profile quick     # the edit loop: no LTO, ~15s incremental
./target/quick/vellum-app
```

`cargo build --release` when you need the diagnostics to run at real speed. Note
`.cargo/config.toml` caps parallelism at `jobs = 4` for small machines — raise it if
yours is bigger.

## The loop

```bash
cargo test --workspace                    # 2,417 tests
cargo clippy --workspace --all-targets    # must be zero warnings
```

Both must be green. Two caveats are in [RULES.md](RULES.md#verification) and will
otherwise confuse you: `glass_budget.rs` is GPU wall-clock and fails under parallel
load, and the reference-board tests skip cleanly on a fresh clone because the exports
are not in the repository.

## What makes a change land

**A measured claim beats an intention.** "This should be faster" is not evidence;
`--hud` and `--exit-after 20` produce a number. If you changed rendering, say what the
frame time was before and after, on what board, at what zoom.

**A/B your test against the unfixed build.** The single most common failure mode in this
codebase's history is a test that was green before the fix and green after it. Comment
out your change and confirm the new test goes red. If it does not, it is testing
something else. Say in the PR that you did this — it is the strongest signal a reviewer
gets.

**Use the diagnostics for anything the input layer touches.** A dialog, a paste, and a
live gesture cannot be photographed by an unattended run, so the app carries `--demo`,
`--show`, `--open-dialog` and `--screenshot` precisely for this. A unit test that
synthesises an event *downstream* of the layer that drops it will pass through the bug.
See trap 9.

**Match the surrounding code.** Comment density here is unusually high and deliberately
so: comments explain *why*, especially where the obvious approach is wrong. If you
remove a comment, be sure you are not removing the reason someone will re-introduce the
bug it prevents.

## Commits and pull requests

- **Present tense, saying what changed and why.** `Snap guides clip to the viewport
  before dashing`, not `fix snapping`. The body is where the reasoning goes.
- **No AI is ever listed as an author, co-author, or contributor.** No
  `Co-Authored-By:` trailer naming a model or tool, no "generated with" line, in commits
  or in PR descriptions. Use whatever tools you like; the log records people. See
  [RULES.md](RULES.md#️-attribution).
- **One concern per PR.** A scrub, a refactor and a feature in one diff is three reviews
  wearing a trench coat.
- **Fill in the template.** It is six lines and it is the checklist a reviewer would ask
  you for anyway.

By contributing you agree your work is licensed under the same dual MIT / Apache-2.0
terms as the project.

## Where to start

- **[`docs/features/README.md`](docs/features/README.md)** — the parity catalogue. Rows
  marked as gaps are real, scoped work.
- **The "Known defects" material in [RULES.md](RULES.md)** — measured, not remembered,
  and each one names what is actually missing.
- **`grep 'fn gap(' crates/vellum-app/src/actions.rs`** — every place the app currently
  answers "not implemented yet" with a toast. Each is a self-describing task.

## Reporting a bug

Include the version (`Velm ▸ About`, or the commit), your OS version, and what you did
in the order you did it. If it involves a board, **do not attach the board** — describe
the shape of it instead. Boards are personal.

If the app crashed, there is a flight recorder: `crates/vellum-app/src/flight.rs` writes
what the app was doing before it died. Say whether it left anything behind.
