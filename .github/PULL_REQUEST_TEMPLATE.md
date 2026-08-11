<!--
  Thanks for the PR. CONTRIBUTING.md has the full guide; this is the short version.
  Delete any line that does not apply.
-->

## What this changes

<!-- One or two sentences. What is different afterwards, and why. -->

## How it was verified

<!--
  A measured claim beats an intention. If you touched rendering or performance, give the
  numbers and say on what board and at what zoom. If you touched anything the input layer
  reaches, say which `--demo` / `--show` fixture you drove it through.
-->

## Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets` is at **zero warnings**
- [ ] I **A/B'd the new test against the unfixed build** and confirmed it goes red
      (the most common failure here is a test that was green before the fix and after it)
- [ ] No board data, board names, account identifiers, or personal URLs in the diff
- [ ] Nothing in this change can delete, truncate or corrupt an existing board
      (see [Rule Zero](../RULES.md#-rule-zero--boards-must-survive-every-change))
- [ ] **No AI is listed as an author, co-author or contributor** — no `Co-Authored-By:`
      trailer naming a model or tool, no "generated with" line
