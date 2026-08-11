//! Inspects a Miro clipboard payload.
//!
//! Deliberately a separate binary from the app: it lets the decoder be proven
//! against real copied data before any UI depends on it, and it is the tool to
//! reach for when Miro changes their format.
//!
//! ```text
//! # macOS: read the clipboard's HTML flavour directly
//! osascript -e 'the clipboard as «class HTML»' | cargo run --bin miro-peek -- --applescript-hex
//! cargo run --bin miro-peek -- clipboard.html
//! cargo run --bin miro-peek -- clipboard.html --json   # full widget dump
//! ```

use anyhow::{Context, Result, bail};
use std::io::Read as _;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let want_json = args.iter().any(|a| a == "--json");
    let from_applescript = args.iter().any(|a| a == "--applescript-hex");
    let path = args.iter().find(|a| !a.starts_with("--"));

    // `.rtb` is a different beast from a clipboard payload: an archive we read
    // assets out of, never board content. Handled here so one tool covers both
    // halves of an import.
    if let Some(p) = path.filter(|p| p.ends_with(".rtb")) {
        return peek_rtb(p);
    }
    // Miro's SVG export: the independent oracle every import is checked against.
    if let Some(p) = path.filter(|p| p.ends_with(".svg")) {
        return peek_svg(p);
    }

    let mut input = String::new();
    match path {
        Some(p) => input = std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
        None => {
            std::io::stdin().read_to_string(&mut input).context("reading stdin")?;
        }
    }

    // `osascript -e 'the clipboard as «class HTML»'` prints `«data HTML<hex>»`.
    if from_applescript {
        input = decode_applescript_hex(&input)?;
    }

    let Some(board) = vellum_import::import_clipboard(&input)? else {
        bail!(
            "no Miro payload found — copy objects in Miro first.\n\
             (Looked for a `data-meta` attribute containing the `miro-data-v1` marker.)"
        );
    };

    if want_json {
        println!("{}", serde_json::to_string_pretty(&board.widgets)?);
        return Ok(());
    }

    println!("source board : {}", board.source_board_id.as_deref().unwrap_or("(unknown)"));
    println!("byte shift   : {}", board.byte_shift);
    println!();
    print!("{}", board.report);

    // Cross-check against Miro's SVG export of the same board. The two formats
    // come from different Miro code paths, so agreement is strong evidence the
    // import is complete — and a mismatch localises the bug.
    if let Some(svg) = args.iter().position(|a| a == "--oracle").and_then(|i| args.get(i + 1)) {
        let export = vellum_import::svg::read_file(svg)?;
        let counts: std::collections::BTreeMap<String, usize> =
            board.report.counts.iter().cloned().collect();
        let discrepancies = export.inventory.compare(&counts);
        println!();
        if discrepancies.is_empty() {
            println!("oracle       : ✓ agrees with {svg}");
        } else {
            println!("oracle       : {} discrepancy/ies vs {svg}", discrepancies.len());
            for d in &discrepancies {
                println!("               {d}");
            }
        }
    }

    // The bounding box is a quick sanity check that positions decoded sensibly.
    if let Some(bounds) = bounds(&board.widgets) {
        let (min_x, min_y, max_x, max_y) = bounds;
        println!();
        println!(
            "extent       : {:.0} x {:.0} px  (x {:.0}..{:.0}, y {:.0}..{:.0})",
            max_x - min_x,
            max_y - min_y,
            min_x,
            max_x,
            min_y,
            max_y
        );
    }
    Ok(())
}

/// Summarises a `.rtb` backup: identity, recoverable assets, and — stated plainly —
/// the encrypted entries that make board content unrecoverable from this file.
fn peek_rtb(path: &str) -> Result<()> {
    use std::collections::BTreeMap;

    let archive = vellum_import::rtb::RtbArchive::open(path)?;
    println!("board        : {}", archive.board.name);
    println!("miro id      : {}", archive.board.id);
    println!("assets       : {}", archive.asset_count());

    let mut by_ext: BTreeMap<&str, usize> = BTreeMap::new();
    let mut infected = 0usize;
    for r in archive.resources() {
        *by_ext.entry(r.extension.as_str()).or_default() += 1;
        infected += usize::from(r.infected);
    }
    let mut exts: Vec<_> = by_ext.into_iter().collect();
    exts.sort_by(|a, b| b.1.cmp(&a.1));
    println!(
        "by extension : {}",
        exts.iter().map(|(e, n)| format!("{n} {e}")).collect::<Vec<_>>().join(", ")
    );
    if infected > 0 {
        println!("infected     : {infected} (will be refused)");
    }

    let encrypted = archive.encrypted_entries();
    if !encrypted.is_empty() {
        println!();
        println!("encrypted    : {}", encrypted.join(", "));
        println!("               Board content is not recoverable from this file — Miro holds");
        println!("               the key. Use the clipboard importer for structure; this");
        println!("               archive supplies original-resolution assets. See");
        println!("               docs/02-miro-formats.md.");
    }
    Ok(())
}

/// Summarises a Miro SVG export — the ground truth an import is measured against.
fn peek_svg(path: &str) -> Result<()> {
    let e = vellum_import::svg::read_file(path)?;
    println!("extent          : {:.2} x {:.2} px", e.extent.width, e.extent.height);
    let i = &e.inventory;
    println!("stickies        : {}", i.stickies);
    for (color, n) in &i.sticky_colors {
        println!("                  {n:>5}  {color}");
    }
    println!("frames          : {}", i.frames);
    println!("shape-el. rects : {}  (widget backgrounds, NOT shape widgets)", i.shape_element_rects);
    println!("link previews   : {}", i.link_previews);
    println!("embeds          : {}", i.embeds);
    println!("arrowheads      : {}", i.connector_arrowheads);
    println!("ink paths       : {}", i.ink_paths);
    println!("text strings    : {}", i.text_strings);
    println!("embedded images : {}", i.embedded_images);
    Ok(())
}

fn bounds(widgets: &[vellum_import::Widget]) -> Option<(f64, f64, f64, f64)> {
    let mut it = widgets.iter().map(|w| (w.placement.x, w.placement.y));
    let (x, y) = it.next()?;
    Some(it.fold((x, y, x, y), |(a, b, c, d), (x, y)| {
        (a.min(x), b.min(y), c.max(x), d.max(y))
    }))
}

/// Unwraps AppleScript's `«data HTML<hex>»` representation.
fn decode_applescript_hex(s: &str) -> Result<String> {
    let s = s.trim();
    let hex = s
        .strip_prefix("«data HTML")
        .and_then(|r| r.strip_suffix('»'))
        .context("expected AppleScript output of the form «data HTML…»")?;
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair)?, 16).context("invalid hex in AppleScript output")
        })
        .collect::<Result<_>>()?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
