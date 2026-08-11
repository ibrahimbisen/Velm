//! The benchmark: 100 boards × 500 items, measured end to end.
//!
//! ```text
//! cargo run --release -p vellum-search --example corpus_benchmark
//! ```
//!
//! # Why a synthetic corpus, and how it is kept honest
//!
//! The reference board in `docs/02-miro-formats.md` is one board. This crate has to
//! hold **20–100** of them, and there is no way to obtain 99 more real ones. So the
//! corpus is synthesised — but every distribution in it is taken from the real
//! board rather than invented, because a corpus of uniform random words would make
//! the index look far better than it is:
//!
//! - **Widget kinds** are in the reference board's own proportions: 219 ink, 122
//!   image, 91 link preview, 46 text, 44 sticky, 41 embed, 18 connector, 12 frame
//!   out of 596.
//! - **Colours** follow the board's stickies: 43 of 44 are `#fff79e` and one is
//!   `#ff9e9e`, so one colour dominates and the facet lists are lopsided, which is
//!   the case that stresses `colour:` filtering.
//! - **Vocabulary** has a Zipf head and a long tail. Real text has a handful of
//!   words that appear everywhere and thousands that appear once or twice — part
//!   numbers, revision codes, compounds. That skew is the whole shape of the
//!   problem: the head is what makes some posting lists enormous, and the tail is
//!   what makes the term dictionary large enough for substring and fuzzy scans to
//!   cost anything at all. A corpus with a small uniform vocabulary makes both look
//!   free, which is the commonest way a search benchmark lies.
//!
//! One departure from the reference board, and it is deliberately in the harsher
//! direction: **every item here carries text**, as the brief specifies (100 × 500 ×
//! ~20 characters). On a real board more than half the widgets — all 219 ink
//! strokes, all 122 images — have none, so a real corpus of this size would have a
//! smaller dictionary and shorter posting lists than this one.
//!
//! Item ids are deliberately unfriendly: `counter@peer` strings in the shape
//! `vellum_doc::ItemId` produces, not small integers, so string interning and
//! comparison are paying their real cost.
//!
//! # What is measured
//!
//! Build, save, load, and query latency at p50/p99 over eight query shapes chosen
//! to cover both the common case and the worst one — including the single-character
//! prefix that a search box sees on the first keystroke of *every* query anybody
//! types, which is the real worst case and the one most benchmarks omit.
//!
//! The bar is stated in the brief: **interactive search must stay under 10ms.** The
//! program prints PASS or FAIL against it per query shape and exits non-zero if any
//! shape misses, so this is a check rather than a report.

use std::time::{Duration, Instant};

use vellum_search::{Colour, Index, Item, Matching, Query};

const BOARDS: usize = 100;
const ITEMS_PER_BOARD: usize = 500;
/// The brief's bar for interactive search.
const INTERACTIVE_BUDGET: Duration = Duration::from_millis(10);

fn main() {
    let items = synthesise();
    let characters: usize = items.iter().map(|item| item.text.chars().count()).sum();
    let with_text = items.iter().filter(|item| !item.text.is_empty()).count();

    println!("CORPUS");
    row("boards", &BOARDS.to_string());
    row("items", &items.len().to_string());
    row("items with text", &format!("{with_text} ({:.0}%)", percent(with_text, items.len())));
    row("mean chars per texted item", &format!("{:.1}", characters as f64 / with_text as f64));

    let started = Instant::now();
    let mut index = Index::build(items.clone());
    let build = started.elapsed();

    let started = Instant::now();
    let bytes = index.to_bytes();
    let encode = started.elapsed();

    let path = std::env::temp_dir().join("vellum-search-benchmark.vsx");
    let started = Instant::now();
    index.save(&path).expect("writing the index");
    let save = started.elapsed();

    let started = Instant::now();
    let reloaded = Index::load(&path).expect("reading the index back");
    let load = started.elapsed();
    assert_eq!(reloaded.stats(), index.stats(), "a reload must be indistinguishable");

    let stats = index.stats();
    println!("\nINDEX");
    row("distinct terms", &stats.terms.to_string());
    row("postings", &stats.postings.to_string());
    row("  mean per term", &format!("{:.1}", stats.postings as f64 / stats.terms as f64));
    row("token positions", &stats.positions.to_string());

    println!("\nTIMING");
    row("build", &millis(build));
    row("  per item", &format!("{:>8.1} us", build.as_secs_f64() * 1e6 / items.len() as f64));
    row("encode", &millis(encode));
    row("save (encode + fsync + rename)", &millis(save));
    row("load", &millis(load));

    println!("\nSIZE ON DISK");
    row("index", &bytes_label(bytes.len()));
    row("  per item", &format!("{:>8.0} B", bytes.len() as f64 / items.len() as f64));
    row("raw text in the corpus", &bytes_label(characters));
    row("ids, kinds, tags and text", &bytes_label(document_bytes(&items)));

    println!("\nINCREMENTAL UPDATE");
    let edit = time_edits(&mut index, &items);
    row("one sticky edited", &micros(edit.one));
    row("  vs. a full rebuild", &format!("{:>8.0}x faster", build.as_secs_f64() / edit.one.as_secs_f64()));
    row("1000 sequential edits, mean", &micros(edit.mean));
    row("unchanged upsert", &micros(edit.unchanged));
    row("re-import of one board (500 items)", &millis(edit.board));

    println!("\nQUERY LATENCY  (1000 runs each, cold scratch buffer every run)");
    println!("  {:<34} {:>9} {:>9} {:>7}", "SHAPE", "P50", "P99", "HITS");
    let mut failed = Vec::new();
    for shape in shapes() {
        let measurement = measure(&index, &shape);
        let verdict = if measurement.p99 <= INTERACTIVE_BUDGET { "PASS" } else { "FAIL" };
        if measurement.p99 > INTERACTIVE_BUDGET {
            failed.push(shape.label);
        }
        println!(
            "  {:<34} {:>9} {:>9} {:>7}  {verdict}",
            shape.label,
            micros(measurement.p50),
            micros(measurement.p99),
            measurement.hits,
        );
    }

    println!();
    if failed.is_empty() {
        println!("PASS  every query shape stayed under the {INTERACTIVE_BUDGET:?} interactive budget at p99.");
    } else {
        println!("FAIL  over the {INTERACTIVE_BUDGET:?} interactive budget at p99: {}", failed.join(", "));
        std::process::exit(1);
    }
}

fn row(label: &str, value: &str) {
    println!("  {label:<34} {value:>12}");
}

fn percent(part: usize, whole: usize) -> f64 {
    part as f64 * 100.0 / whole as f64
}

fn millis(duration: Duration) -> String {
    format!("{:>8.2} ms", duration.as_secs_f64() * 1e3)
}

fn micros(duration: Duration) -> String {
    format!("{:>8.1} us", duration.as_secs_f64() * 1e6)
}

fn bytes_label(count: usize) -> String {
    format!("{:>8.2} MB", count as f64 / (1024.0 * 1024.0))
}

/// What the corpus weighs before a single posting is written — the floor any index
/// that can render its own snippets has to pay.
fn document_bytes(items: &[Item]) -> usize {
    items
        .iter()
        .map(|item| {
            item.board.len()
                + item.item.len()
                + item.kind.len()
                + item.text.len()
                + item.tags.iter().map(String::len).sum::<usize>()
        })
        .sum()
}

// ---------------------------------------------------------------- the corpus

/// The reference board's widget mix, as `(kind, count out of 596)`.
const KIND_MIX: &[(&str, u32)] = &[
    ("ink", 219),
    ("image", 122),
    ("link_preview", 91),
    ("text", 46),
    ("sticky", 44),
    ("embed", 41),
    ("connector", 18),
    ("frame", 12),
    ("document", 2),
    ("group", 1),
];

/// Vocabulary drawn from what the user's boards are actually about — engines,
/// wiring, keyboards — because term length and shape affect prefix and edit-distance
/// costs, and `lorem ipsum` is neither.
const VOCABULARY: &[&str] = &[
    "cooling", "fan", "relay", "coolant", "thermostat", "radiator", "hose", "pump", "water",
    "engine", "bay", "wiring", "harness", "connector", "pin", "ecu", "sensor", "module", "fuse",
    "ground", "loom", "terminal", "voltage", "resistance", "ohm", "crimp", "solder", "shield",
    "intake", "manifold", "turbo", "wastegate", "boost", "charge", "intercooler", "throttle",
    "injector", "rail", "spark", "coil", "plug", "timing", "chain", "guide", "tensioner",
    "gasket", "seal", "oil", "filter", "sump", "dipstick", "level", "pressure", "switch",
    "keyboard", "switch", "keycap", "stabiliser", "plate", "pcb", "firmware", "layout", "matrix",
    "diode", "hotswap", "socket", "lube", "film", "spring", "housing", "stem", "actuation",
    "review", "todo", "done", "blocked", "check", "order", "spare", "torque", "spec", "note",
    "garage", "panel", "mount", "b58", "n55", "bearing", "shroud", "bracket", "assembly",
];

/// How many long-tail words exist to be drawn from. See the module docs.
const TAIL_WORDS: usize = 20_000;

fn synthesise() -> Vec<Item> {
    let mut random = Random::seeded(0x5EA5_C4E5);
    let tail = tail_vocabulary(&mut random);
    let yellow = Colour::parse("#fff79e").unwrap();
    let salmon = Colour::parse("#ff9e9e").unwrap();
    let cyan = Colour::parse("#6fd6e6").unwrap();
    let mut items = Vec::with_capacity(BOARDS * ITEMS_PER_BOARD);

    let mix_total: u32 = KIND_MIX.iter().map(|(_, weight)| weight).sum();

    for board in 0..BOARDS {
        // Board ids in the shape `vellum-store` produces: a slug plus a suffix.
        let board_id = format!("plan-{board:03}-{}", ["engine", "wiring", "keys"][board % 3]);
        for item in 0..ITEMS_PER_BOARD {
            let kind = pick_weighted(KIND_MIX, mix_total, &mut random);
            // `vellum_doc::ItemId` displays as `counter@peer`, and interning those
            // is part of what indexing costs.
            let id = format!("{}@{:016x}", item, 0x7000_0000_0000_0000u64 + board as u64);
            let text = phrase(&mut random, &tail);

            let colour = match kind {
                // 43 of the reference board's 44 stickies are the same yellow.
                "sticky" => Some(if random.below(44) == 0 { salmon } else { yellow }),
                "frame" => Some(cyan),
                _ => None,
            };

            let mut built = Item::new(&board_id, id, kind, text);
            if let Some(colour) = colour {
                built = built.with_colour(colour);
            }
            // A tenth of the items carry a tag, which is roughly how people use them.
            if random.below(10) == 0 {
                built = built.with_tags([["review", "todo", "blocked"][random.below(3) as usize]]);
            }
            items.push(built);
        }
    }
    items
}

/// The long tail: part numbers, suffixed part names and compounds.
///
/// All three shapes are taken from what is actually written on the user's boards.
/// The compounds matter especially: `coolinghose` is why a substring query is worth
/// having at all, and a corpus without any would make substring matching look free.
fn tail_vocabulary(random: &mut Random) -> Vec<String> {
    (0..TAIL_WORDS)
        .map(|_| {
            let base = VOCABULARY[random.below(VOCABULARY.len() as u32) as usize];
            match random.below(3) {
                // Part numbers are eleven digits.
                0 => format!("{}", 11_000_000_000u64 + random.next() % 900_000_000),
                1 => format!("{base}{}", random.below(9999)),
                _ => format!("{base}{}", VOCABULARY[random.below(VOCABULARY.len() as u32) as usize]),
            }
        })
        .collect()
}

/// Roughly twenty characters of text, as the brief specifies.
fn phrase(random: &mut Random, tail: &[String]) -> String {
    let mut out = String::with_capacity(28);
    while out.len() < 18 {
        if !out.is_empty() {
            out.push(' ');
        }
        // Roughly a quarter of tokens come from the tail. Higher would make the
        // dictionary enormous and every posting list a singleton, which is no more
        // realistic than a vocabulary of ninety.
        if random.below(4) == 0 {
            out.push_str(&tail[zipf_index(random, tail.len())]);
        } else {
            out.push_str(VOCABULARY[zipf_index(random, VOCABULARY.len())]);
        }
    }
    out
}

/// An index biased toward the front of a list, so a few entries dominate.
///
/// Sampling the `min` of two uniform draws is a cheap, deterministic way to get
/// that skew without building a cumulative distribution table.
fn zipf_index(random: &mut Random, length: usize) -> usize {
    let a = random.below(length as u32);
    let b = random.below(length as u32);
    a.min(b) as usize
}

fn pick_weighted(table: &[(&'static str, u32)], total: u32, random: &mut Random) -> &'static str {
    let mut roll = random.below(total);
    for (value, weight) in table {
        if roll < *weight {
            return value;
        }
        roll -= weight;
    }
    table[0].0
}

/// xorshift64*. Deterministic, seeded, and no dependency: the corpus must be the
/// same on every run and on every machine or the numbers are not comparable.
struct Random(u64);

impl Random {
    fn seeded(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u32) -> u32 {
        (self.next() >> 33) as u32 % bound
    }
}

// -------------------------------------------------------------- measurements

struct Edits {
    one: Duration,
    mean: Duration,
    unchanged: Duration,
    board: Duration,
}

fn time_edits(index: &mut Index, items: &[Item]) -> Edits {
    let texted: Vec<&Item> = items.iter().filter(|item| !item.text.is_empty()).collect();

    let victim = texted[0];
    let edited = Item::new(&victim.board, &victim.item, &victim.kind, "thermostat housing gasket");
    let started = Instant::now();
    index.upsert(&edited);
    let one = started.elapsed();
    index.upsert(victim);

    let started = Instant::now();
    for (n, item) in texted.iter().take(1000).enumerate() {
        index.upsert(&Item::new(
            &item.board,
            &item.item,
            &item.kind,
            format!("edited {n} coolant hose clamp"),
        ));
    }
    let mean = started.elapsed() / 1000;
    for item in texted.iter().take(1000) {
        index.upsert(*item);
    }

    let started = Instant::now();
    for item in texted.iter().take(1000) {
        index.upsert(*item);
    }
    let unchanged = started.elapsed() / 1000;

    let board = items[0].board.clone();
    let revision: Vec<Item> = items.iter().filter(|item| item.board == board).cloned().collect();
    let started = Instant::now();
    index.replace_board(&board, revision);
    let board_time = started.elapsed();

    Edits { one, mean, unchanged, board: board_time }
}

struct Shape {
    label: &'static str,
    query: Query,
}

fn shapes() -> Vec<Shape> {
    vec![
        Shape { label: "exact word  `cooling`", query: Query::parse("cooling") },
        Shape { label: "two words   `cooling fan`", query: Query::parse("cooling fan") },
        Shape {
            label: "prefix      `coo`",
            query: Query::parse("coo"),
        },
        Shape {
            label: "worst case  `c` (first keystroke)",
            query: Query::parse("c"),
        },
        Shape { label: "phrase      `\"cooling fan\"`", query: Query::parse("\"cooling fan\"") },
        Shape {
            label: "fuzzy       `colling` (2 edits)",
            query: Query::parse("colling").with_matching(Matching::forgiving()),
        },
        Shape { label: "facet       `all yellow stickies`", query: Query::parse("all yellow stickies") },
        Shape { label: "filter      `kind:frame`", query: Query::parse("kind:frame") },
        Shape {
            label: "select all  `kind:sticky` unlimited",
            query: Query::parse("kind:sticky").with_limit(usize::MAX),
        },
        Shape {
            label: "scoped      `cooling board:plan-000-alpha`",
            query: Query::parse("cooling board:plan-000-alpha"),
        },
    ]
}

struct Measurement {
    p50: Duration,
    p99: Duration,
    hits: usize,
}

fn measure(index: &Index, shape: &Shape) -> Measurement {
    const RUNS: usize = 1000;
    // One untimed run so the first measurement is not paying for a cold page.
    let hits = index.search(&shape.query).len();

    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let found = index.search(&shape.query);
        samples.push(started.elapsed());
        // Keep the optimiser from deleting the search it was asked to time.
        std::hint::black_box(&found);
    }
    samples.sort_unstable();
    Measurement { p50: samples[RUNS / 2], p99: samples[RUNS * 99 / 100], hits }
}
