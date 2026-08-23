# The two landing pages that were not shipped

Three directions were written independently and judged on craft, on persuasion, and on whether
they actually work in a browser. `web/home.html` is the one that shipped, built on the winner
with the best ideas from the other two grafted in.

These are kept rather than deleted because they are finished pages, not sketches, and because
the judges' notes name real ideas in each that did not make the cut.

- `home-a.html` — the hero **is** a live board you can draw on. This is the direction that won.
- `home-b.html` — bold editorial: 82px headline, the measured numbers set at the scale of the
  claim they make, no ornament at all.
- `home-c.html` — the switch page, built around the honest Miro comparison.

⚠ None of these is served. `scripts/build-web.sh` copies a fixed list and
`crates/velmd/src/serve.rs` maps `/` to `home.html`; putting one of these in front means
renaming it and naming it there. They are here to be read, not routed.
