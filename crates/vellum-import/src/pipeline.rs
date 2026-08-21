//! The end-to-end import: a Miro clipboard payload becomes a real Vellum board.
//!
//! [`import`] is the whole paste path. Everything before it — [`clipboard`],
//! [`mapper`], [`rtb`], [`svg`] — reads Miro's formats; this is where the result
//! stops being a description of someone else's board and starts being ours.
//!
//! [`clipboard`]: crate::clipboard
//! [`mapper`]: crate::mapper
//! [`rtb`]: crate::rtb
//! [`svg`]: crate::svg
//!
//! # The three things that are easy to get wrong
//!
//! **Hierarchy.** Miro references objects by array index (`_parent.index`, group
//! `items`); Vellum uses a Loro movable tree keyed by [`ItemId`]. Rebuilding the
//! tree is therefore a second pass, after every item exists. Crucially it is a
//! *pure* second pass: [`mapper`](crate::mapper) resolves placements to absolute
//! world coordinates and [`vellum_doc::Placement`] is absolute too, so reparenting
//! must not move a single pixel. There is a test that asserts exactly that.
//!
//! **Assets.** `image` and `document` widgets carry a `resource.id`, never bytes.
//! The bytes come from a `.rtb` alongside the paste, get stored by content hash,
//! and the item records that hash — so a board file never depends on Miro's id
//! namespace. An asset that is referenced but absent is *reported*, never dropped:
//! the item still imports, at the right place, minus its pixels.
//!
//! **Z-order.** The clipboard's array order is the stacking order, and Loro's
//! fractional index preserves it as long as items are created and reparented in
//! that order. See [`rebuild_hierarchy`].
//!
//! # Fidelity
//!
//! [`vellum_doc::ItemKind`] has four variants; Miro hands us eleven types. The gap
//! is bridged by substitution, never by dropping: a frame becomes a text item that
//! still contains its children, a connector becomes an ink polyline drawn between
//! the bounds it was bound to, a link card becomes its title and link as styled
//! text. Every substitution is named, counted and priced in [`ImportOutcome`],
//! because an import that looks complete and is not is the failure that destroys
//! trust fastest. [`DEGRADATIONS`] is the whole list in one place.

use crate::ImportedBoard;
use crate::miro_model::{self as miro, FidelityReport, Widget, WidgetKind};
use crate::rtb::ArchiveSet;
use crate::svg::Discrepancy;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use vellum_doc::{
    Align, Board, CardMode, Color, Crop, ItemId, ItemKind, NewItem, Placement, Point, SpanStyle,
    Style, StyledText, TextSpan,
};
use vellum_store::BlobStore;

/// Every Miro type that cannot be represented exactly yet: what it becomes, and
/// what that substitution costs.
///
/// Kept as one table rather than scattered through the conversion so the fidelity
/// report and the code that produces it cannot drift apart, and so the list of
/// [`ItemKind`] variants still owed to `vellum-doc` is readable at a glance.
const DEGRADATIONS: &[Degradation] = &[
    Degradation {
        miro_type: "group",
        imported_as: "text",
        lost: "nothing but the group's own identity — members are nested under an \
               empty item that acts as the container",
    },
    Degradation {
        miro_type: "connector",
        imported_as: "ink",
        lost: "the endpoint bindings, so it will not re-route when either end moves; \
               also arrowheads, dash pattern, routing mode, jump-overs and captions",
    },
    Degradation {
        miro_type: "embed",
        imported_as: "embed",
        lost: "the live frame — the card carries the title, link, provider and preview \
               image, and clicking it opens the page in a browser",
    },
    Degradation {
        miro_type: "document",
        imported_as: "image",
        lost: "PDF page rendering — the bytes are stored and referenced, but nothing \
               draws them yet",
    },
    Degradation {
        miro_type: "rich_document",
        imported_as: "text",
        lost: "headings and list structure; character formatting survives",
    },
];

/// One row of [`DEGRADATIONS`], with the count filled in for a particular import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Degradation {
    /// The Miro type, as [`WidgetKind::label`] spells it.
    pub miro_type: &'static str,
    /// The [`ItemKind::tag`] it had to become.
    pub imported_as: &'static str,
    /// What did not survive.
    pub lost: &'static str,
}

/// Why an asset's bytes are not in the blob store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetGap {
    /// No `.rtb` was supplied. The clipboard never carries pixels, so nothing
    /// could have been recovered.
    NoArchive,
    /// An archive was supplied but holds no such resource — usually a paste from
    /// one board joined against another board's backup.
    NotInArchive,
    /// The archive holds it but refused or failed to serve it, e.g. Miro flagged
    /// the upload as infected.
    Unreadable(String),
}

impl std::fmt::Display for AssetGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoArchive => f.write_str("no .rtb archive was supplied"),
            Self::NotInArchive => f.write_str("not present in the .rtb archive"),
            Self::Unreadable(why) => write!(f, "unreadable: {why}"),
        }
    }
}

/// An item that imported without its pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingAsset {
    /// Miro's `resource.id` — the join key that failed.
    pub resource_id: String,
    /// The original upload name, when the widget carried one.
    pub name: Option<String>,
    /// Index into the clipboard's objects array, so the widget can be found again.
    pub widget: usize,
    /// The item that was still created, at the right place, holding no asset.
    pub item: ItemId,
    pub gap: AssetGap,
}

/// What an import produced, and what it cost.
///
/// Built to be shown to a user immediately after a paste — see the [`Display`]
/// implementation — and to be assertable in a test without re-deriving anything.
///
/// [`Display`]: std::fmt::Display
#[derive(Debug)]
pub struct ImportOutcome {
    /// Miro's board id, e.g. `"bTBja0JvYXJkSWQ="`. Several pastes sharing it came from
    /// the same board and can be merged.
    pub source_board_id: Option<String>,
    /// The obfuscation offset the payload actually used. Recorded so a change in
    /// Miro's format is visible in logs rather than silent.
    pub byte_shift: u8,
    /// The mapper's report, extended with everything the pipeline learned.
    pub report: FidelityReport,
    /// One id per clipboard object, in array order — so `items[i]` is the item for
    /// `objects[i]`, and the vector itself is the imported z-order.
    pub items: Vec<ItemId>,
    /// Count per Miro widget type, keyed by [`WidgetKind::label`].
    pub counts: BTreeMap<String, usize>,
    /// Miro types this build does not map at all, by their Miro name. Empty on
    /// every board seen so far; non-empty means Miro shipped something new.
    pub unmapped_types: BTreeMap<String, usize>,
    /// Substitutions that were actually made, with counts. A subset of
    /// [`DEGRADATIONS`] plus anything discovered while converting.
    pub degraded: Vec<(Degradation, usize)>,
    pub missing_assets: Vec<MissingAsset>,
    /// Assets whose bytes reached the blob store.
    pub assets_recovered: usize,
    /// Total bytes stored, before deduplication against what was already there.
    pub asset_bytes: u64,
    /// Wall time spent on assets alone — ZIP extraction, hashing and the blob write.
    ///
    /// Reported so a paste can be apportioned rather than guessed at. On the reference
    /// board this is most of the import, which is what justifies moving it off the frame.
    pub asset_time: std::time::Duration,
    /// Result of [`ImportOutcome::cross_check`], if it was run. `Some(vec![])`
    /// means the oracle agreed.
    pub oracle: Option<Vec<Discrepancy>>,
}

impl ImportOutcome {
    /// Number of items added to the board.
    pub fn total(&self) -> usize {
        self.items.len()
    }

    /// Widgets that came through with no loss at all.
    pub fn lossless(&self) -> usize {
        let degraded: usize = self.degraded.iter().map(|(_, n)| n).sum();
        self.total() - degraded - self.missing_assets.len()
    }

    /// Cross-checks the import against Miro's SVG export of the same board.
    ///
    /// The two formats are produced by different Miro code paths, so agreement is
    /// the strongest correctness signal available offline — and a disagreement
    /// localises the bug to a widget type. Discrepancies are also folded into
    /// [`FidelityReport::warnings`] so they reach the user through the ordinary
    /// summary rather than only through a caller that thought to look.
    ///
    /// Optional because most pastes have no SVG to check against; skipping it
    /// changes nothing about what was imported.
    pub fn cross_check(&mut self, svg_export: impl AsRef<Path>) -> Result<&[Discrepancy]> {
        let path = svg_export.as_ref();
        let export = crate::svg::read_file(path)?;
        let found = export.inventory.compare(&self.counts);
        for d in &found {
            self.report.warnings.push(format!("oracle ({}) disagrees — {d}", path.display()));
        }
        Ok(self.oracle.insert(found))
    }
}

/// The summary shown after a paste: what arrived, what it cost, what is missing.
impl std::fmt::Display for ImportOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} items imported from Miro", self.total())?;
        match &self.source_board_id {
            Some(id) => writeln!(f, " (board {id})")?,
            None => writeln!(f)?,
        }

        let mut counts: Vec<_> = self.counts.iter().collect();
        counts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (label, n) in counts {
            writeln!(f, "  {n:>5}  {label}")?;
        }

        if self.assets_recovered > 0 || !self.missing_assets.is_empty() {
            let total = self.assets_recovered + self.missing_assets.len();
            writeln!(
                f,
                "  assets: {} of {total} recovered ({:.1} MB) in {:.0} ms",
                self.assets_recovered,
                self.asset_bytes as f64 / 1_048_576.0,
                self.asset_time.as_secs_f64() * 1000.0
            )?;
        }

        if !self.degraded.is_empty() {
            writeln!(f, "  reduced fidelity — Velm has no matching item kind yet:")?;
            for (d, n) in &self.degraded {
                writeln!(f, "  {n:>5}  {} → {}", d.miro_type, d.imported_as)?;
                writeln!(f, "         lost: {}", d.lost)?;
            }
        }

        for (miro_type, n) in &self.unmapped_types {
            writeln!(f, "  {n:>5}  {miro_type}: unrecognised Miro type, imported as a placeholder")?;
        }

        for missing in dedup_gaps(&self.missing_assets) {
            writeln!(f, "  missing asset {missing}")?;
        }

        match &self.oracle {
            Some(d) if d.is_empty() => writeln!(f, "  oracle: agrees")?,
            Some(d) => {
                for d in d {
                    writeln!(f, "  oracle: {d}")?;
                }
            }
            None => {}
        }

        for warning in &self.report.warnings {
            writeln!(f, "  warning: {warning}")?;
        }
        Ok(())
    }
}

/// Imports a Miro clipboard payload onto `board`.
///
/// `archive` is an optional `.rtb` backup opened alongside the paste; it is the
/// only source of image and document bytes, since the clipboard carries
/// references. Without it those items still import — placed, sized and counted —
/// and are listed in [`ImportOutcome::missing_assets`].
///
/// `blobs` is taken by shared reference rather than `&mut`: [`BlobStore`] is
/// content-addressed with no in-memory state, so exclusive access would buy
/// nothing and would stop an import running while anything else reads assets.
///
/// `Ok(None)` means the HTML was not Miro's — the ordinary case when pasting from
/// anywhere else, and not an error. `Err` means it *was* Miro's and could not be
/// imported; the board is left as the partial import found it, which is recoverable
/// with one undo because the whole import is a single undo group.
pub fn import(
    clipboard_html: &str,
    archive: Option<&mut ArchiveSet>,
    blobs: &BlobStore,
    board: &mut Board,
) -> Result<Option<ImportOutcome>> {
    let Some(decoded) = crate::import_clipboard(clipboard_html)? else {
        return Ok(None);
    };
    import_widgets(&decoded, archive, blobs, board).map(Some)
}

/// Imports widgets that have already been decoded and mapped.
///
/// Split out from [`import`] so a caller that has to paginate a very large board
/// — Miro's clipboard has shown no cap, but the format is undocumented — can
/// decode fragments itself and feed them in. Placements are absolute, so fragments
/// compose without fixups.
pub fn import_widgets(
    source: &ImportedBoard,
    archive: Option<&mut ArchiveSet>,
    blobs: &BlobStore,
    board: &mut Board,
) -> Result<ImportOutcome> {
    import_widgets_with(source, archive, blobs, board, None)
}

/// [`import_widgets`] with the assets already in hand.
///
/// The half of an import that has to happen on the thread owning the document, once
/// [`prefetch_assets`] has done the half that does not. Measured on the reference board:
/// this part is **~50 ms** of a 1,774 ms paste, so a caller that prefetches on a worker and
/// calls in here keeps its window painting for all but a few frames of the wait.
///
/// Passing `None` is the everything-here path and is what [`import_widgets`] does.
pub fn import_widgets_with(
    source: &ImportedBoard,
    archive: Option<&mut ArchiveSet>,
    blobs: &BlobStore,
    board: &mut Board,
    ready: Option<PrefetchedAssets>,
) -> Result<ImportOutcome> {
    // One paste is one undo step, however many items it turns into. Closed on the
    // error path too, so a failed import cannot leave the group open and fuse the
    // user's next edit into it.
    board.begin_undo_group()?;
    let outcome = build(source, archive, blobs, board, ready);
    board.end_undo_group();
    outcome
}

/// How many threads pull assets out of the backups at once.
///
/// Four rather than the core count: the work is ZIP inflate plus a staged write and a
/// rename per asset, so it is as much disk as CPU, and the machine this is developed on is
/// an 8 GB laptop where a fan-out that large competes with the frame it is trying to keep
/// free. Four measured well and leaves the UI thread a core.
const ASSET_THREADS: usize = 4;

/// Pulls every asset the board asks for out of the backups, in parallel.
///
/// # Why this exists
///
/// Measured on the reference board — 596 widgets, a 110.5 MB backup, a cold blob store —
/// the import took **2,564 ms and 2,484 ms of it was assets**: 97%. Everything else, the
/// 596 CRDT inserts included, came to about 80 ms. So the whole of a Miro paste's cost is
/// ZIP inflate, BLAKE3 and a file write per asset, and all of it is independent per asset.
///
/// Safe to run concurrently against one [`BlobStore`]: it is content-addressed, every write
/// stages to a uniquely-named temporary and is published by an atomic rename, so two threads
/// racing on the *same* asset both write the same bytes to the same name and the loser's
/// rename is a no-op. That property is the store's, not this function's — it is why writing
/// blobs from several threads needs no lock.
///
/// A reader cannot be shared, because a `.rtb` archive owns a seek position, so each worker
/// opens the backups again from their paths ([`ArchiveSet::paths`]).
/// Assets already pulled out of the backups, ready for a conversion that must not block.
///
/// Resource id → its blob hash and size, or why it could not be had. Produced by
/// [`prefetch_assets`] on whatever thread the caller likes, and handed to
/// [`import_widgets_with`] on the one that owns the document.
pub type PrefetchedAssets = HashMap<String, std::result::Result<(String, u64), AssetGap>>;

/// One worker's answers: the resource id, and either its blob hash and size or why not.
type PrefetchedBatch = Vec<(String, std::result::Result<(String, u64), AssetGap>)>;

fn prefetch(
    ids: &[String],
    paths: &[std::path::PathBuf],
    blobs: &BlobStore,
) -> PrefetchedAssets {
    let mut resolved = HashMap::new();
    if ids.is_empty() || paths.is_empty() {
        return resolved;
    }

    // Distinct, because one resource is commonly referenced by many widgets and fetching it
    // twice is the thing the sequential path's cache already prevented.
    let mut wanted: Vec<&str> = ids.iter().map(String::as_str).collect();
    wanted.sort_unstable();
    wanted.dedup();

    let threads = ASSET_THREADS.min(wanted.len());
    let chunk = wanted.len().div_ceil(threads);
    let answers: Vec<PrefetchedBatch> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = wanted
                .chunks(chunk)
                .map(|slice| {
                    scope.spawn(move || {
                        // One reader per worker. An archive that fails to open here is not
                        // fatal: every id it would have answered simply reports a gap, which
                        // is the same answer the sequential path gives for a missing backup.
                        let mut set = ArchiveSet::new();
                        for path in paths {
                            if let Err(error) = set.add(path) {
                                log::warn!("prefetch: reopening {}: {error}", path.display());
                            }
                        }
                        slice
                            .iter()
                            .map(|id| {
                                let outcome = match set.asset_bytes(id) {
                                    Ok(Some(bytes)) => blobs
                                        .put(&bytes)
                                        // The length travels with the hash: the paste's
                                        // report states how many megabytes were recovered,
                                        // and only the thread holding the bytes knows.
                                        // Dropping it here reported 0 MB on the path that
                                        // recovers everything, which a test caught.
                                        .map(|hash| (hash.to_hex().to_string(), bytes.len() as u64))
                                        .map_err(|error| AssetGap::Unreadable(error.to_string())),
                                    Ok(None) => Err(AssetGap::NotInArchive),
                                    Err(error) => Err(AssetGap::Unreadable(error.to_string())),
                                };
                                ((*id).to_owned(), outcome)
                            })
                            .collect()
                    })
                })
                .collect();
            handles.into_iter().filter_map(|handle| handle.join().ok()).collect()
        });

    for batch in answers {
        resolved.extend(batch);
    }
    resolved
}

/// Which archive resources a board's widgets ask for.
///
/// Runs the real `convert` with **no archive attached**, so the answer comes from the same
/// code that will ask for them again — there is no second list of "which widgets have
/// assets" to fall out of step with the first. Everything it converts is thrown away; only
/// the requests are kept. Measured at **2 ms** for 596 widgets, which is what makes it
/// affordable as a planning step before the expensive part.
#[must_use]
pub fn requested_assets(source: &ImportedBoard, blobs: &BlobStore) -> Vec<String> {
    let geometry = Geometry::resolve(&source.widgets);
    let mut probe = Assets::new(None, blobs);
    for (index, widget) in source.widgets.iter().enumerate() {
        let _ = convert(widget, index, &geometry, &mut probe);
    }
    probe.requested
}

/// Pulls the named assets out of the backups. **Safe to call off the UI thread.**
///
/// This is the whole point of the split: it is the part of an import that takes seconds —
/// 2,012 ms of a 2,076 ms paste, measured — and it touches no document. Everything it needs
/// is owned or cheap to clone, so a caller can run it on a worker and keep painting.
#[must_use]
pub fn prefetch_assets(
    ids: &[String],
    paths: &[std::path::PathBuf],
    blobs: &BlobStore,
) -> PrefetchedAssets {
    prefetch(ids, paths, blobs)
}

fn build(
    source: &ImportedBoard,
    archive: Option<&mut ArchiveSet>,
    blobs: &BlobStore,
    board: &mut Board,
    ready: Option<PrefetchedAssets>,
) -> Result<ImportOutcome> {
    let widgets = &source.widgets;
    let geometry = Geometry::resolve(widgets);
    let hierarchy = Hierarchy::resolve(widgets);

    let mut report = source.report.clone();
    report.warnings.extend(hierarchy.warnings.iter().cloned());

    // **Assets first, in parallel, and only then the conversion.**
    //
    // The conversion asks for an asset the moment it meets a widget that needs one, which
    // made the import a sequence of 205 stop-the-world extractions interleaved with cheap
    // work. Doing them together up front turns 2.5 seconds of that into a fraction, and
    // costs one extra pass over the widgets to find out *which* assets are wanted.
    //
    // The probe pass runs the real `convert` with no archive attached, so the answer comes
    // from the same code that will ask for them again — there is no second list of "which
    // widgets have assets" to fall out of step. Its output is thrown away; only
    // `Assets::requested` is kept.
    let paths = archive.as_ref().map(|set| set.paths()).unwrap_or_default();
    let prefetch_started = std::time::Instant::now();
    let mut probe_time = std::time::Duration::ZERO;
    // A caller that has already done this on a worker hands the answers in; one that has
    // not pays for them here, on whatever thread it is on. Both paths converge on the same
    // seeding below, so the conversion cannot tell the difference.
    let prefetched = if let Some(ready) = ready {
        ready
    } else if paths.is_empty() {
        HashMap::new()
    } else {
        let probe_started = std::time::Instant::now();
        let mut probe = Assets::new(None, blobs);
        for (index, widget) in widgets.iter().enumerate() {
            let _ = convert(widget, index, &geometry, &mut probe);
        }
        probe_time = probe_started.elapsed();
        prefetch(&probe.requested, &paths, blobs)
    };
    let prefetch_time = prefetch_started.elapsed();
    log::info!(
        "import: prefetched {} asset(s) in {:.0} ms ({:.0} ms of it the probe pass)",
        prefetched.len(),
        prefetch_time.as_secs_f64() * 1000.0,
        probe_time.as_secs_f64() * 1000.0
    );

    let mut assets = Assets::new(archive, blobs);
    // Seeded, so the conversion below finds every asset already in hand and touches no
    // archive. Anything the prefetch could not resolve is left out rather than cached as a
    // failure, so the sequential path still gets its turn and reports the real gap.
    for (id, outcome) in prefetched {
        if outcome.is_ok() {
            assets.adopt(id, outcome);
        }
    }
    let mut items = Vec::with_capacity(widgets.len());
    let mut missing_assets = Vec::new();
    let mut flattened_lists = 0usize;

    // Pass one: every widget becomes a top-level item, in clipboard array order.
    // Creating flat and nesting afterwards is what keeps z-order intact even when
    // a parent appears later in the array than its children.
    for (index, widget) in widgets.iter().enumerate() {
        let converted = convert(widget, index, &geometry, &mut assets)?;
        flattened_lists += usize::from(converted.flattened_list);

        let item = board.add(
            NewItem::new(converted.kind, geometry.placements[index])
                .with_style(converted.style),
        )?;
        items.push(item);

        if let Some((asset, gap)) = converted.gap {
            missing_assets.push(MissingAsset {
                resource_id: asset.id,
                name: asset.name,
                widget: index,
                item,
                gap,
            });
        }
    }

    // Pass two: nest. Nothing here touches a placement.
    report.warnings.extend(rebuild_hierarchy(board, &hierarchy.parent, &items));

    let assets_recovered = assets.recovered;
    // The prefetch is where the asset work happens now; `assets.elapsed` only sees
    // whatever the sequential path still had to do, which is normally nothing.
    let asset_time = assets.elapsed + prefetch_time;
    let asset_bytes = assets.bytes;

    let counts: BTreeMap<String, usize> = report.counts.iter().cloned().collect();
    let unmapped_types = widgets
        .iter()
        .filter_map(|w| match &w.kind {
            WidgetKind::Unsupported { miro_type } => Some(miro_type.clone()),
            _ => None,
        })
        .fold(BTreeMap::new(), |mut acc, t| {
            *acc.entry(t).or_default() += 1;
            acc
        });

    let mut degraded: Vec<(Degradation, usize)> = DEGRADATIONS
        .iter()
        .filter_map(|d| counts.get(d.miro_type).map(|n| (*d, *n)))
        .collect();
    if flattened_lists > 0 {
        degraded.push((
            Degradation {
                miro_type: "sticky/text with a list",
                imported_as: "text",
                lost: "bullet and numbering structure — Velm's styled text has no list model",
            },
            flattened_lists,
        ));
    }

    report.missing_assets = dedup_gaps(&missing_assets);

    Ok(ImportOutcome {
        source_board_id: source.source_board_id.clone(),
        byte_shift: source.byte_shift,
        report,
        items,
        counts,
        unmapped_types,
        degraded,
        missing_assets,
        assets_recovered,
        asset_time,
        asset_bytes,
        oracle: None,
    })
}

// ----- hierarchy ----------------------------------------------------------

/// Miro's parent references, validated into something a movable tree will accept.
struct Hierarchy {
    /// Parent per widget, as an index into the clipboard's objects array.
    parent: Vec<Option<usize>>,
    warnings: Vec<String>,
}

impl Hierarchy {
    /// Merges the two ways Miro expresses containment.
    ///
    /// A widget names its container through `_parent.index`. A **group** works the
    /// other way round: it has no `widgetData` and no `_parent` of its own, and
    /// instead lists its members in `items`. On the reference board, group 595
    /// lists widgets 266 and 267 while both of those name frame 263 as their
    /// parent — so the real shape is `frame 263 > group 595 > {266, 267}`, and the
    /// group inherits the container its members were already in.
    fn resolve(widgets: &[Widget]) -> Self {
        let n = widgets.len();
        let mut warnings = Vec::new();
        let mut parent: Vec<Option<usize>> = widgets
            .iter()
            .enumerate()
            .map(|(i, w)| w.parent_index.filter(|&p| p < n && p != i))
            .collect();

        for (group, widget) in widgets.iter().enumerate() {
            let WidgetKind::Group { children } = &widget.kind else { continue };
            let members: Vec<usize> =
                children.iter().copied().filter(|&c| c < n && c != group).collect();

            // Read the inherited container *before* the members are re-pointed at
            // the group, or it is gone.
            if parent[group].is_none() {
                parent[group] = members.iter().find_map(|&c| parent[c]).filter(|&p| p != group);
            }
            for member in members {
                parent[member] = Some(group);
            }
        }

        // A cycle would make `Board::reparent` fail item by item and leave the
        // hierarchy half-built, so it is broken here, once, and reported.
        //
        // States: 0 unvisited, 1 on the path being walked, 2 known to reach a root.
        let mut state = vec![0u8; n];
        for start in 0..n {
            let mut path = Vec::new();
            let mut current = start;
            loop {
                match state[current] {
                    0 => {
                        state[current] = 1;
                        path.push(current);
                    }
                    1 => {
                        warnings.push(format!(
                            "widget {current} is its own ancestor in Miro's `_parent` chain; \
                             detached it to the top level"
                        ));
                        parent[current] = None;
                        break;
                    }
                    _ => break,
                }
                match parent[current] {
                    Some(next) => current = next,
                    None => break,
                }
            }
            for node in path {
                state[node] = 2;
            }
        }

        Self { parent, warnings }
    }
}

/// Nests every item under its Miro parent, preserving z-order.
///
/// Ascending array order is not incidental: Loro's `mov` appends to the end of the
/// new parent's child list, so moving children in the order Miro listed them
/// reproduces that order among siblings. Reparenting out of the root list leaves
/// the remaining roots in their relative order for the same reason.
///
/// Placements are untouched, and must be: [`mapper`](crate::mapper) already
/// resolved them to absolute world space and [`vellum_doc::Placement`] is absolute,
/// so nesting is pure bookkeeping. A reparent that moved something would be a bug
/// visible as an item jumping the instant it is grouped.
fn rebuild_hierarchy(
    board: &mut Board,
    parent: &[Option<usize>],
    items: &[ItemId],
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, parent) in parent.iter().enumerate() {
        let Some(parent) = *parent else { continue };
        if let Err(error) = board.reparent(items[index], Some(items[parent])) {
            // Cycles are already broken above, so this means Loro rejected the
            // move for a reason we did not anticipate. Leaving the item at the top
            // level is visible and fixable; aborting the paste is not.
            warnings.push(format!(
                "could not nest widget {index} inside widget {parent} ({error}); \
                 it stays at the top level"
            ));
        }
    }
    warnings
}

// ----- geometry -----------------------------------------------------------

/// An axis-aligned box in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Rect {
    fn from_center(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            min_x: x - width / 2.0,
            min_y: y - height / 2.0,
            max_x: x + width / 2.0,
            max_y: y + height / 2.0,
        }
    }

    /// A point at a normalised 0–1 position across the box, which is how Miro
    /// expresses a connector's attachment: `{x: 1, y: 0.5}` is the right edge,
    /// vertically centred.
    fn at(&self, (u, v): (f64, f64)) -> (f64, f64) {
        (
            self.min_x + u * (self.max_x - self.min_x),
            self.min_y + v * (self.max_y - self.min_y),
        )
    }

    fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }
}

/// Placements and world bounds for every widget.
///
/// Three kinds have no `_position` of their own and are derived instead: ink from
/// its stroke points, connectors from the widgets their endpoints bind to, and
/// groups from the union of their members. That ordering is a dependency chain, so
/// it is resolved in three passes rather than lazily.
struct Geometry {
    placements: Vec<Placement>,
    /// Stroke points recentred on the item's placement, for the kinds stored as
    /// ink. Computed here because connectors need the same treatment as `paint`.
    strokes: Vec<Vec<Point>>,
}

impl Geometry {
    fn resolve(widgets: &[Widget]) -> Self {
        let mut placements: Vec<Placement> = widgets.iter().map(direct_placement).collect();
        let mut strokes: Vec<Vec<Point>> = vec![Vec::new(); widgets.len()];

        for (index, widget) in widgets.iter().enumerate() {
            if let WidgetKind::Ink(ink) = &widget.kind {
                let (size, points) = recenter(&ink.points);
                placements[index].width = size.0;
                placements[index].height = size.1;
                strokes[index] = points;
            }
        }

        let bounds_of = |p: &Placement| {
            let (w, h) = p.scaled_size();
            Rect::from_center(p.x, p.y, w, h)
        };
        let mut bounds: Vec<Rect> = placements.iter().map(bounds_of).collect();

        // Connectors have `_position: null` — their geometry *is* their endpoints.
        for (index, widget) in widgets.iter().enumerate() {
            let WidgetKind::Connector { start, end, .. } = &widget.kind else { continue };
            let a = endpoint(start, &bounds);
            let b = endpoint(end, &bounds);
            let (a, b) = match (a, b) {
                (Some(a), Some(b)) => (a, b),
                // A half-bound connector still deserves to exist somewhere real.
                (Some(p), None) | (None, Some(p)) => (p, p),
                (None, None) => continue,
            };
            let center = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            placements[index] = Placement::new(
                center.0,
                center.1,
                (b.0 - a.0).abs(),
                (b.1 - a.1).abs(),
            );
            strokes[index] = vec![
                Point::new(a.0 - center.0, a.1 - center.1),
                Point::new(b.0 - center.0, b.1 - center.1),
            ];
            bounds[index] = bounds_of(&placements[index]);
        }

        // Groups arrive with no placement at all. Giving them their members' extent
        // makes the container node selectable and hit-testable instead of a
        // zero-size item stranded at the origin.
        for (index, widget) in widgets.iter().enumerate() {
            let WidgetKind::Group { children } = &widget.kind else { continue };
            let extent = children
                .iter()
                .filter(|&&c| c < bounds.len() && c != index)
                .map(|&c| bounds[c])
                .reduce(Rect::union);
            let Some(extent) = extent else { continue };
            placements[index] = Placement::new(
                (extent.min_x + extent.max_x) / 2.0,
                (extent.min_y + extent.max_y) / 2.0,
                extent.max_x - extent.min_x,
                extent.max_y - extent.min_y,
            );
            bounds[index] = extent;
        }

        Self { placements, strokes }
    }
}

/// Converts a mapped placement, resolving the sizes Miro leaves implicit.
///
/// `scale` stays separate from `width`/`height` rather than being multiplied in:
/// [`vellum_doc::Placement::scaled_size`] applies it, and folding it here would
/// double-count it — and lose the distinction between a 200px item at 2× and a
/// 400px item at 1×, which matters the moment the user resizes one.
fn direct_placement(widget: &Widget) -> Placement {
    let p = &widget.placement;
    Placement {
        x: p.x,
        y: p.y,
        scale: p.scale,
        rotation: p.rotation,
        width: p.width.unwrap_or(0.0),
        height: p.height.unwrap_or(0.0),
    }
}

/// Moves stroke points from Miro's origin-at-the-bounding-box-corner convention to
/// Vellum's, where points are relative to the item's centre.
///
/// Returns the box's size in the same *unscaled* units as the points, because
/// [`Placement::scale`] is applied to both by the renderer.
fn recenter(points: &[(f64, f64)]) -> ((f64, f64), Vec<Point>) {
    let Some(&(first_x, first_y)) = points.first() else {
        return ((0.0, 0.0), Vec::new());
    };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first_x, first_y, first_x, first_y);
    for &(x, y) in points {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let (center_x, center_y) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let recentred = points.iter().map(|&(x, y)| Point::new(x - center_x, y - center_y)).collect();
    ((max_x - min_x, max_y - min_y), recentred)
}

fn endpoint(end: &miro::ConnectorEnd, bounds: &[Rect]) -> Option<(f64, f64)> {
    let target = end.target.filter(|&t| t < bounds.len())?;
    Some(bounds[target].at(end.anchor))
}

// ----- assets -------------------------------------------------------------

/// Resolves Miro `resource.id`s to blob-store content hashes.
struct Assets<'a> {
    archive: Option<&'a mut ArchiveSet>,
    blobs: &'a BlobStore,
    /// Miro resource id → BLAKE3 hex. Many widgets can share one resource and the
    /// reference archive is 109MB, so each asset is read, hashed and stored once.
    resolved: HashMap<String, std::result::Result<String, AssetGap>>,
    recovered: usize,
    bytes: u64,
    /// Every resource asked for, in first-use order.
    ///
    /// The list is what makes the parallel prefetch possible without a second copy of
    /// "which widgets reference an asset": that rule lives in `convert`, and re-deriving it
    /// here would be two sources of truth for something only a real board exercises. A probe
    /// pass over the widgets with no archive attached fills this in, and every id it names is
    /// one `convert` genuinely asked for.
    requested: Vec<String>,
    /// Wall time spent pulling assets out of the archive and into the blob store.
    ///
    /// Instrumentation, kept: on a real 596-widget board this is the great majority of a
    /// paste, and it is the number that decides whether moving asset work off the UI thread
    /// is worth the architecture. Measuring it beat guessing — an earlier round guessed the
    /// item inserts were the cost and was wrong.
    elapsed: std::time::Duration,
}

impl<'a> Assets<'a> {
    fn new(archive: Option<&'a mut ArchiveSet>, blobs: &'a BlobStore) -> Self {
        Self {
            archive,
            blobs,
            resolved: HashMap::new(),
            recovered: 0,
            bytes: 0,
            elapsed: std::time::Duration::ZERO,
            requested: Vec::new(),
        }
    }

    /// The blob hash for an asset, or why there isn't one.
    ///
    /// A gap is never an error: a paste with no matching backup is an ordinary
    /// situation, and the item it belongs to still imports.
    /// Takes an already-resolved asset, from the parallel prefetch.
    ///
    /// Counted into `recovered` **and `bytes`** exactly as a sequential fetch would be.
    ///
    /// The byte count is why `prefetch` hands back a length beside the hash: only the worker
    /// that inflated the asset knows how big it was, and an earlier version of this dropped
    /// it and reported *"0.0 MB"* on the very path that recovers everything. Two tests
    /// caught it, which is the argument for their existing.
    fn adopt(&mut self, id: String, outcome: std::result::Result<(String, u64), AssetGap>) {
        let outcome = match outcome {
            Ok((hash, bytes)) => {
                self.recovered += 1;
                self.bytes += bytes;
                Ok(hash)
            }
            Err(gap) => Err(gap),
        };
        self.resolved.insert(id, outcome);
    }

    fn hash_of(&mut self, id: &str) -> std::result::Result<String, AssetGap> {
        if let Some(known) = self.resolved.get(id) {
            return known.clone();
        }
        let outcome = self.fetch(id);
        self.resolved.insert(id.to_owned(), outcome.clone());
        outcome
    }

    fn fetch(&mut self, id: &str) -> std::result::Result<String, AssetGap> {
        let started = std::time::Instant::now();
        let answer = self.fetch_now(id);
        self.elapsed += started.elapsed();
        answer
    }

    fn fetch_now(&mut self, id: &str) -> std::result::Result<String, AssetGap> {
        // Recorded before the archive is consulted, so a probe pass with no archive still
        // names everything the board wants.
        self.requested.push(id.to_owned());
        let Some(archive) = self.archive.as_deref_mut() else {
            return Err(AssetGap::NoArchive);
        };
        let bytes = match archive.asset_bytes(id) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(AssetGap::NotInArchive),
            Err(error) => return Err(AssetGap::Unreadable(error.to_string())),
        };
        match self.blobs.put(&bytes) {
            Ok(hash) => {
                self.recovered += 1;
                self.bytes += bytes.len() as u64;
                Ok(hash.to_hex())
            }
            Err(error) => Err(AssetGap::Unreadable(error.to_string())),
        }
    }
}

/// Collapses per-item gaps into one line per resource, so a board that reuses a
/// missing image forty times reports it once.
fn dedup_gaps(missing: &[MissingAsset]) -> Vec<String> {
    let mut seen: BTreeMap<&str, (&MissingAsset, usize)> = BTreeMap::new();
    for gap in missing {
        let entry = seen.entry(&gap.resource_id).or_insert((gap, 0));
        entry.1 += 1;
    }
    seen.into_values()
        .map(|(gap, times)| {
            let name = gap.name.as_deref().unwrap_or("(unnamed)");
            let used = if times == 1 { String::new() } else { format!(", used {times}×") };
            format!("{} [{}]{used}: {}", name, gap.resource_id, gap.gap)
        })
        .collect()
}

// ----- widget → item ------------------------------------------------------

/// One widget, converted.
struct Converted {
    kind: ItemKind,
    style: Style,
    /// Set when the widget referenced an asset we could not supply.
    gap: Option<(miro::AssetRef, AssetGap)>,
    /// The widget's rich text contained a list, which a flat span model cannot
    /// carry. Counted so the report can say so.
    flattened_list: bool,
}

impl Converted {
    fn new(kind: ItemKind) -> Self {
        Self { kind, style: Style::default(), gap: None, flattened_list: false }
    }

    fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// Turns one Miro widget into the closest [`ItemKind`] Vellum has.
///
/// Where the fit is exact — sticky, text, ink, image — this is a translation.
/// Where it is not, the substitution is the one that keeps the most user-visible
/// meaning, and [`DEGRADATIONS`] records the price. Nothing is ever skipped: an
/// item that renders as nothing still holds its place in the tree and the z-order,
/// so a later `ItemKind` variant is a re-render rather than a re-import.
fn convert(
    widget: &Widget,
    index: usize,
    geometry: &Geometry,
    assets: &mut Assets<'_>,
) -> Result<Converted> {
    Ok(match &widget.kind {
        WidgetKind::Sticky { html, background, style } => {
            let rich = richtext::from_html(html, base_span(style));
            Converted {
                kind: ItemKind::Sticky { text: rich.text, background: color(*background) },
                style: item_style(style),
                gap: None,
                flattened_list: rich.flattened_list,
            }
        }

        WidgetKind::Text { html, style } => {
            let rich = richtext::from_html(html, base_span(style));
            Converted {
                kind: ItemKind::Text { text: rich.text },
                style: item_style(style),
                gap: None,
                flattened_list: rich.flattened_list,
            }
        }

        WidgetKind::Ink(ink) => Converted::new(ItemKind::Ink {
            points: geometry.strokes[index].clone(),
            // Miro carries stroke opacity as a separate key; Vellum folds it into
            // the colour so a renderer can never apply one without the other.
            color: color(ink.color).map(|c| match ink.opacity {
                Some(opacity) => c.with_opacity(opacity),
                None => c,
            }),
            thickness: ink.thickness.unwrap_or(1.0),
        }),

        WidgetKind::Image { asset, crop } => {
            let (asset_id, gap) = resolve_asset(asset, assets);
            Converted {
                kind: ItemKind::Image { asset_id, crop: crop.map(convert_crop) },
                style: Style::default(),
                gap,
                flattened_list: false,
            }
        }

        // No `ItemKind::Document` yet. The PDF still reaches the blob store and the
        // item still references it by hash, so gaining that variant is a re-render
        // of boards that already exist rather than a re-import from Miro.
        WidgetKind::Document { asset } => {
            let (asset_id, gap) = resolve_asset(asset, assets);
            Converted {
                kind: ItemKind::Image { asset_id, crop: None },
                style: Style::default(),
                gap,
                flattened_list: false,
            }
        }

        // A frame becomes a frame. It used to become a text item carrying only the
        // name, which meant an imported board had no frames on it at all: the Text arm
        // of the painter pushes no geometry, so all twelve of the reference board's
        // frames were an invisible title and nothing else.
        WidgetKind::Frame { title, background, order, speaker_notes } => Converted::new(
            ItemKind::Frame {
                title: StyledText::plain(title.as_str()),
                order: *order,
                speaker_notes: speaker_notes.clone(),
            },
        )
        .with_style(Style {
            // Miro's own frame background, which on the reference board is #ffffff.
            // `None` inherits the theme's frame fill, which is also white — so a frame
            // that never had a colour and one that was explicitly white agree.
            fill: color(*background),
            ..Style::default()
        }),

        // Connectors become the polyline they were already drawn as. The binding is
        // what is lost, not the line.
        WidgetKind::Connector { style, .. } => Converted::new(ItemKind::Ink {
            points: geometry.strokes[index].clone(),
            color: color(style.color),
            thickness: style.thickness.unwrap_or(1.0),
        }),

        // Both become the card kinds the document has always had and nothing ever built.
        //
        // They used to become `ItemKind::Text` holding a `link_card` blob — title, URL and
        // description as three styled lines — which is why 132 of the reference board's 596
        // widgets drew as paragraphs of grey text instead of as cards, and why both were
        // counted as *degraded*. `ItemKind::LinkPreview` and `ItemKind::Embed` existed the
        // whole time, and `draw.rs` had card painters for both. The same shape as the frame
        // bug in `CLAUDE.md`'s feedback 12: a decoder that read the widget perfectly and then
        // handed it to the wrong variant.
        //
        // The **provider** is derived from the URL's host rather than fetched, so an imported
        // board names its sites offline and on the first frame.
        WidgetKind::LinkPreview { title, url, description, image, visual_type } => {
            Converted::new(link_kind(LinkParts {
                is_embed: false,
                title,
                url,
                description,
                provider: &None,
                html: &None,
                thumbnail: link_thumbnail(image.as_ref(), assets),
                mode: card_mode(*visual_type),
            }))
        }

        WidgetKind::Embed { title, url, description, provider, html, image } => {
            Converted::new(link_kind(LinkParts {
                is_embed: true,
                title,
                url,
                description,
                provider,
                html,
                thumbnail: link_thumbnail(image.as_ref(), assets),
                // An embed has no `visualType`; Miro always draws it as a full card.
                mode: CardMode::Card,
            }))
        }

        WidgetKind::RichDocument { ops } => {
            Converted::new(ItemKind::Text { text: richtext::from_delta(ops) })
        }

        // An empty text item is the only container Vellum has. It draws nothing,
        // which is exactly what a group looks like.
        WidgetKind::Group { .. } => Converted::new(ItemKind::Text { text: StyledText::default() }),

        // Placed, sized, counted and named in the report — everything except drawn.
        // The untouched Miro JSON is still on the widget, so nothing is lost that a
        // later build cannot recover.
        WidgetKind::Unsupported { .. } => {
            Converted::new(ItemKind::Text { text: StyledText::default() })
        }
    })
}

fn resolve_asset(
    asset: &miro::AssetRef,
    assets: &mut Assets<'_>,
) -> (String, Option<(miro::AssetRef, AssetGap)>) {
    match assets.hash_of(&asset.id) {
        Ok(hash) => (hash, None),
        // An empty asset id is how the document says "this image has no bytes yet",
        // which is the truth rather than a dangling reference to something absent.
        Err(gap) => (String::new(), Some((asset.clone(), gap))),
    }
}

fn convert_crop(crop: miro::Crop) -> Crop {
    Crop { x: crop.x, y: crop.y, width: crop.width, height: crop.height }
}

fn color(rgb: Option<miro::Rgb>) -> Option<Color> {
    rgb.map(|miro::Rgb(r, g, b)| Color::rgb(r, g, b))
}

/// Widget-level typography. The split matches Miro's own: `ffn`/`fs`/`ta`/`lh` are
/// properties of the widget, while bold and links vary inside its rich text.
fn item_style(style: &miro::TextStyle) -> Style {
    Style {
        font_family: style.font_family.clone(),
        font_size: style.font_size,
        text_color: color(style.color),
        align: style.align.map(|a| match a {
            miro::Align::Left => Align::Left,
            miro::Align::Center => Align::Center,
            miro::Align::Right => Align::Right,
        }),
        line_height: style.line_height,
        opacity: None,
        // Typography only. A frame's `bc` fill is resolved where frames are, not
        // here, and an outline belongs to shapes, which the mapper does not decode
        // yet — see the note on `WidgetKind::Unsupported`.
        fill: None,
        stroke: None,
        stroke_width: None,
        // Miro does have a lock flag on a widget; the clipboard payload has never been
        // observed carrying one, so importing everything unlocked is what the capture
        // supports. `docs/02-miro-formats.md` is the place to record it if one turns up.
        locked: false,
    }
}

/// Miro's widget-level `b`/`i`/`u`/`s` flags apply to the whole widget, so they
/// seed every span; tags inside the HTML add to them rather than replacing them.
fn base_span(style: &miro::TextStyle) -> SpanStyle {
    SpanStyle {
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strikethrough: style.strikethrough,
        link: None,
        color: None,
    }
}

/// A link card as text: the title, then the URL as a real link, then the blurb.
///
/// Not a faithful rendering of the card — there is no thumbnail and no chrome —
/// but it keeps every piece of information the card carried, and the link stays
/// clickable rather than becoming a string that happens to look like a URL.
/// The card kind for a Miro `preview` or `embed`.
///
/// One function for both because the only difference is which variant holds them — Miro's split
/// is about whether the URL's provider offers oEmbed, not about how the card looks — and the
/// **provider name is derived from the host** either way, so an imported board names its sites
/// with no network and on the first frame. A later fetch can improve the title and add a
/// preview image; nothing here waits for one.
struct LinkParts<'a> {
    is_embed: bool,
    title: &'a Option<String>,
    url: &'a Option<String>,
    description: &'a Option<String>,
    provider: &'a Option<String>,
    html: &'a Option<String>,
    thumbnail: Option<String>,
    mode: CardMode,
}

fn link_kind(parts: LinkParts<'_>) -> ItemKind {
    let LinkParts { is_embed, title, url, description, provider, html, thumbnail, mode } = parts;
    // **Miro's openGraph strings are raw HTML, and were reaching the canvas raw.**
    //
    // `mapper::str_field` is a pass-through, and `decode_entities` was wired into the
    // rich-text path only — so a title stored as `For 2018&#43; Acme M5` drew those five
    // characters literally on the board. The same board's *prose* has always been decoded,
    // which is why this went unnoticed: the defect is confined to the metadata Miro fetched
    // on the user's behalf rather than to anything they typed.
    //
    // The URL is decoded too, and that one is not cosmetic: Miro escapes `&` as `&amp;` in
    // query strings, so an undecoded address is a *different* address, and the ↗ badge would
    // open a page the user did not link to.
    let decode = |value: &Option<String>| value.as_deref().map(richtext::decode_entities);
    let (title, url, description) = (decode(title), decode(url), decode(description));
    // Miro's own name for the site wins, because it is what the user sees on the board —
    // it says "YouTube" where the host-derived guess also says "YouTube", but says
    // "Partsdb" and "Instrutec" where a bare host would be less certain. The host fallback
    // still answers for every `preview`, none of which carry a provider at all.
    let provider = decode(provider).or_else(|| url.as_deref().and_then(vellum_link::provider_for));
    if is_embed {
        // Built directly rather than through `ItemKind::embed`, which exists to default the
        // three fields an importer could not fill — and now it can fill two of them.
        ItemKind::Embed {
            title,
            url,
            description,
            provider,
            html: html.clone(),
            thumbnail,
            favicon: None,
            mode,
        }
    } else {
        ItemKind::LinkPreview {
            title,
            url,
            description,
            provider,
            thumbnail,
            // Still a fetch's job: Miro stores no favicon, it renders one from the host.
            favicon: None,
            mode,
        }
    }
}

/// The blob hash for a link card's preview image, if Miro's stored copy is in the archive.
///
/// **A miss is not a gap.** An image widget with no bytes draws an empty rectangle and is
/// counted as degraded; a link card with no picture is still a complete card — title, site,
/// blurb and link — and the fetch pool fills the picture in later. Recording it as a missing
/// asset would put "82 without their assets" in the paste toast for cards that are about to
/// heal themselves, which reads as a failed import.
fn link_thumbnail(image: Option<&miro::AssetRef>, assets: &mut Assets<'_>) -> Option<String> {
    let image = image?;
    if image.id.is_empty() {
        return None;
    }
    assets.hash_of(&image.id).ok()
}

/// Miro's `visualType` → the document's own card form.
///
/// Measured on the reference board rather than guessed, because Miro documents none of it:
/// type **2** (43 cards) always carries a preview image and is the tallest at a median
/// 250×361, type **1** (18) always carries one at 250×203, and type **0** (30) is the odd
/// one — only 3 of its 30 have an image at all, so it is the form for a page that offered
/// no picture. So 2 is the large preview and the other two are the ordinary card; a card
/// asked to show an image it does not have already falls back to the `Card` layout in the
/// painter, which is what makes mapping 0 here safe.
///
/// `CardMode::Link`, the one-line row, is deliberately not produced: Miro stores a collapsed
/// link as a **text widget carrying an `<a href>`**, not as a `preview` at all, which is why
/// 46 of the board's text items are bare URLs.
fn card_mode(visual_type: Option<i64>) -> CardMode {
    match visual_type {
        Some(2) => CardMode::Large,
        _ => CardMode::Card,
    }
}

// ----- rich text ----------------------------------------------------------

/// Miro's two rich-text formats, converted to [`StyledText`].
///
/// `sticker` and `text` carry rich-text HTML; `structured_document` carries Quill
/// delta ops. Both are real formats with real formatting in them, and showing the
/// user their own markup — or flattening it to a bare `String` — would be a
/// visible, immediate loss on the most common widget types on a board.
mod richtext {
    use super::{Color, SpanStyle, StyledText, TextSpan};
    use crate::miro_model::DeltaOp;

    /// Tags after which a line break belongs. `br` is handled separately because
    /// it is a break rather than a container.
    const BLOCK_TAGS: &[&str] = &["p", "div", "li", "ol", "ul", "h1", "h2", "h3", "h4", "h5", "h6"];

    /// Tags whose only job is to carry list structure, which a flat span model
    /// cannot represent. Seeing one is what the fidelity report counts.
    const LIST_TAGS: &[&str] = &["ol", "ul", "li"];

    /// How deep the tag walk will follow nesting.
    ///
    /// Real content nests about four deep (`ol > li > span > a`). The bound exists
    /// because the walk is recursive and the input is an untrusted clipboard: a
    /// megabyte of nested `<div>`s would otherwise overflow the stack on a paste.
    /// Text below the bound is still emitted; only its formatting is dropped.
    const MAX_NESTING: usize = 64;

    pub(super) struct RichText {
        pub text: StyledText,
        pub flattened_list: bool,
    }

    /// Parses Miro's rich-text HTML into styled spans.
    ///
    /// A real parser rather than a regex: the payload contains nested `<strong>`
    /// inside `<li>` inside `<ol>`, `<a href>` with query strings full of
    /// entity-escaped `=` and `&`, and Quill's own `<span class="ql-ui">` markers.
    /// Regex handles none of that without silently corrupting text.
    pub(super) fn from_html(html: &str, base: SpanStyle) -> RichText {
        let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) else {
            // Unparseable markup still has readable text in it; showing that beats
            // showing nothing, and beats showing raw tags.
            return RichText { text: StyledText::plain(decode_entities(html)), flattened_list: false };
        };

        let mut walker = Walker { parser: dom.parser(), spans: Vec::new(), flattened_list: false };
        for child in dom.children() {
            walker.walk(child, &base, 0);
        }

        // Miro's editor terminates content with an empty paragraph, so a note
        // reading "fan" arrives as `<p>fan</p><p><br /></p>`. Trailing blank lines
        // are invisible on the board and would make a round-trip compare unequal.
        let mut spans = walker.spans;
        while let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end_matches('\n');
            if trimmed.len() == last.text.len() {
                break;
            }
            last.text.truncate(trimmed.len());
            if !last.text.is_empty() {
                break;
            }
            spans.pop();
        }

        RichText { text: StyledText::from_spans(spans), flattened_list: walker.flattened_list }
    }

    struct Walker<'a, 'b> {
        parser: &'a tl::Parser<'b>,
        spans: Vec<TextSpan>,
        flattened_list: bool,
    }

    impl Walker<'_, '_> {
        fn walk(&mut self, handle: &tl::NodeHandle, style: &SpanStyle, depth: usize) {
            let Some(node) = handle.get(self.parser) else { return };
            match node {
                tl::Node::Raw(text) => {
                    self.spans.push(TextSpan::new(decode_entities(&text.as_utf8_str()), style.clone()));
                }
                tl::Node::Comment(_) => {}
                // Past the bound, keep the words and lose the markup: the text of a
                // pathologically nested document is still the user's content.
                //
                // `children().all()` is a flat slice of every descendant, which is
                // the point — `HTMLTag::inner_text` recurses, so using it here
                // would reintroduce the overflow this branch exists to prevent.
                tl::Node::Tag(tag) if depth >= MAX_NESTING => {
                    let text: String = tag
                        .children()
                        .all(self.parser)
                        .iter()
                        .filter_map(|node| node.as_raw())
                        .map(|raw| decode_entities(&raw.as_utf8_str()))
                        .collect();
                    self.spans.push(TextSpan::new(text, style.clone()));
                }
                tl::Node::Tag(tag) => {
                    let name = tag.name().as_utf8_str().to_ascii_lowercase();
                    if name == "br" {
                        self.spans.push(TextSpan::new("\n", style.clone()));
                        return;
                    }
                    if LIST_TAGS.contains(&name.as_str()) {
                        self.flattened_list = true;
                    }
                    if BLOCK_TAGS.contains(&name.as_str()) {
                        self.break_line();
                    }

                    let mut style = style.clone();
                    match name.as_str() {
                        "b" | "strong" => style.bold = true,
                        "i" | "em" => style.italic = true,
                        "u" | "ins" => style.underline = true,
                        "s" | "strike" | "del" => style.strikethrough = true,
                        "a" => {
                            if let Some(Some(href)) = tag.attributes().get("href") {
                                style.link = Some(decode_entities(&href.as_utf8_str()));
                            }
                        }
                        _ => {}
                    }
                    if let Some(Some(css)) = tag.attributes().get("style")
                        && let Some(found) = css_color(&css.as_utf8_str())
                    {
                        style.color = Some(found);
                    }

                    for child in tag.children().top().iter() {
                        self.walk(child, &style, depth + 1);
                    }
                }
            }
        }

        /// Starts a new line, unless one has already started. Emitting the break
        /// *before* a block rather than after it is what stops `<p>Brakes</p>` —
        /// far and away the commonest sticky on a real board — from importing with
        /// a trailing newline.
        fn break_line(&mut self) {
            if self.spans.iter().any(|s| !s.text.is_empty())
                && !self.spans.last().is_some_and(|s| s.text.ends_with('\n'))
            {
                self.spans.push(TextSpan::plain("\n"));
            }
        }
    }

    /// Miro's `structured_document`, which is Quill delta ops.
    pub(super) fn from_delta(ops: &[DeltaOp]) -> StyledText {
        StyledText::from_spans(ops.iter().map(|op| {
            let mut style = SpanStyle::default();
            if let Some(attributes) = &op.attributes {
                let flag = |key: &str| attributes.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
                style.bold = flag("bold");
                style.italic = flag("italic");
                style.underline = flag("underline");
                style.strikethrough = flag("strike");
                style.link =
                    attributes.get("link").and_then(|v| v.as_str()).map(str::to_owned);
                style.color = attributes.get("color").and_then(|v| v.as_str()).and_then(hex_color);
            }
            TextSpan::new(op.insert.as_str(), style)
        }))
    }

    /// Resolves the entities Miro's rich text actually contains.
    ///
    /// On the reference board that is `&#61;` and `&#43;` inside link query
    /// strings, plus `&amp;`, `&#34;` and `&#39;` in prose. `&amp;` is resolved
    /// last so `&amp;lt;` does not collapse to `<`.
    pub(super) fn decode_entities(s: &str) -> String {
        if !s.contains('&') {
            return s.to_owned();
        }

        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(at) = rest.find('&') {
            out.push_str(&rest[..at]);
            rest = &rest[at..];

            // **Clamped to a char boundary, not just to 12 bytes.** `rest` starts at the `&`,
            // so byte 12 lands wherever it lands — and an entity followed by CJK puts it inside
            // a three-byte character, where `rest[..12]` aborts the process. Reachable from any
            // Alibaba listing whose title contains an `&`, which is most of them, and newly
            // reachable from *every* imported card since `link_kind` began routing its title,
            // url, description and provider through here.
            let mut window = rest.len().min(12);
            while window > 0 && !rest.is_char_boundary(window) {
                window -= 1;
            }
            let end = rest[..window].find(';');
            let Some(end) = end else {
                out.push('&');
                rest = &rest[1..];
                continue;
            };

            match resolve_entity(&rest[1..end]) {
                Some(resolved) => {
                    out.push(resolved);
                    rest = &rest[end + 1..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
        out.push_str(rest);
        // Ampersands are resolved after the rest so a doubly-escaped entity
        // survives as text rather than being decoded twice.
        out.replace("&amp;", "&")
    }

    fn resolve_entity(body: &str) -> Option<char> {
        match body {
            "lt" => return Some('<'),
            "gt" => return Some('>'),
            "quot" => return Some('"'),
            "apos" => return Some('\''),
            "nbsp" => return Some('\u{a0}'),
            // Left for the final pass; see `decode_entities`.
            "amp" => return None,
            _ => {}
        }
        let digits = body.strip_prefix('#')?;
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse().ok()?,
        };
        char::from_u32(code)
    }

    /// Pulls a colour out of a `style` attribute, e.g. `"color: #3578ff"`.
    fn css_color(css: &str) -> Option<Color> {
        let at = css.find("color")?;
        let value = css[at..].split(':').nth(1)?;
        hex_color(value.split(';').next()?.trim())
    }

    /// `#rgb` or `#rrggbb`, the two forms Quill and Miro emit.
    pub(super) fn hex_color(hex: &str) -> Option<Color> {
        let digits = hex.trim().strip_prefix('#')?;
        let byte = |at: usize, len: usize| {
            let slice = digits.get(at..at + len)?;
            let value = u8::from_str_radix(slice, 16).ok()?;
            Some(if len == 1 { value * 17 } else { value })
        };
        let len = match digits.len() {
            3 => 1,
            6 => 2,
            _ => return None,
        };
        Some(Color::rgb(byte(0, len)?, byte(len, len)?, byte(2 * len, len)?))
    }
}

// ----- convenience --------------------------------------------------------

/// Imports a clipboard payload into a brand-new board, titled from the archive.
///
/// The shape of "paste into an empty board", which is how an import from a `.rtb`
/// plus a clipboard copy actually starts. The board's own name only exists in the
/// archive — the clipboard payload carries an id but no title.
pub fn import_to_new_board(
    clipboard_html: &str,
    archive: Option<&mut ArchiveSet>,
    blobs: &BlobStore,
) -> Result<Option<(Board, ImportOutcome)>> {
    let title = archive.as_ref().and_then(|a| a.sole_board_name()).map(str::to_owned);
    let mut board = Board::new();
    let Some(outcome) = import(clipboard_html, archive, blobs, &mut board)? else {
        return Ok(None);
    };
    if let Some(title) = title {
        board.set_title(&title).context("naming the imported board")?;
    }
    Ok(Some((board, outcome)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtb::RtbArchive;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};

    /// Wraps objects the way Miro's clipboard does, so tests exercise the real
    /// entry point rather than a shortcut past the decoder.
    fn clipboard(objects: Value) -> String {
        let payload =
            json!({ "boardId": "bTBja0JvYXJkSWQ=", "version": 2, "data": { "objects": objects } })
                .to_string();
        let shifted: Vec<u8> = payload.bytes().map(|b| b.wrapping_sub(197)).collect();
        format!(
            "<span data-meta=\"&lt;--(miro-data-v1){}(/miro-data-v1)--&gt;\"></span>",
            STANDARD.encode(&shifted)
        )
    }

    fn widget(ty: &str, id: i64, json: Value) -> Value {
        json!({ "id": id, "type": 14, "widgetData": { "type": ty, "json": json } })
    }

    fn blobs() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        (dir, store)
    }

    fn run(objects: Value) -> (Board, ImportOutcome, tempfile::TempDir) {
        let (dir, store) = blobs();
        let mut board = Board::new();
        let outcome = import(&clipboard(objects), None, &store, &mut board).unwrap().unwrap();
        (board, outcome, dir)
    }

    /// A frame 400×100 centred at (100, 200), with a sticky offset (10, 20) from
    /// its top-left. The numbers match `mapper`'s own resolution test.
    fn frame_with_child() -> Value {
        json!([
            widget("frame", 0, json!({
                "_position": { "offsetPx": { "x": 100.0, "y": 200.0 }, "schema": "canvasOffsetPx" },
                "width": 400.0, "height": 100.0, "text": "Sensors"
            })),
            widget("sticker", 1, json!({
                "_position": { "offsetPx": { "x": 10.0, "y": 20.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": 0 }, "size": { "width": 199, "height": 228 },
                "text": "<p>fan</p>", "style": "{\"sbc\":16775070}"
            }))
        ])
    }

    /// The invariant the whole hierarchy pass rests on. Miro's coordinates are
    /// resolved to absolute before any of this runs and Vellum's placements are
    /// absolute too, so nesting an item must move it by exactly zero.
    #[test]
    fn reparenting_does_not_move_anything() {
        let (board, outcome, _dir) = run(frame_with_child());

        let frame = board.item(outcome.items[0]).unwrap();
        let sticky = board.item(outcome.items[1]).unwrap();

        assert_eq!(sticky.parent, Some(outcome.items[0]), "the sticky must be inside the frame");
        // Resolved absolutely: (100, 200) − (400, 100)/2 + (10, 20).
        assert_eq!((sticky.placement.x, sticky.placement.y), (-90.0, 170.0));
        assert_eq!((frame.placement.x, frame.placement.y), (100.0, 200.0));

        // And re-stating it as the property rather than the numbers: every item is
        // exactly where the mapper put it, nesting or no nesting.
        let mapped = crate::import_clipboard(&clipboard(frame_with_child())).unwrap().unwrap();
        for (widget, id) in mapped.widgets.iter().zip(&outcome.items) {
            let placed = board.item(*id).unwrap().placement;
            assert_eq!((placed.x, placed.y), (widget.placement.x, widget.placement.y));
        }
    }

    #[test]
    fn sticky_html_becomes_styled_text_and_keeps_its_colour() {
        let (board, outcome, _dir) = run(frame_with_child());
        let ItemKind::Sticky { text, background } = board.item(outcome.items[1]).unwrap().kind
        else {
            panic!("expected a sticky")
        };
        assert_eq!(text.to_plain(), "fan", "the paragraph wrapper is markup, not content");
        assert_eq!(background.unwrap().to_hex(), "#fff79e");
    }

    /// A group is the other way Miro expresses containment, and it points *at* its
    /// members instead of being pointed at. `frame > group > members` has to come
    /// out of that, and this is the exact shape of object 595 on the real board.
    #[test]
    fn groups_nest_inside_the_frame_their_members_were_already_in() {
        let (board, outcome, _dir) = run(json!([
            widget("frame", 0, json!({
                "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
                "width": 1000.0, "height": 1000.0, "text": "both drive trains integrated"
            })),
            widget("sticker", 1, json!({
                "_position": { "offsetPx": { "x": 100.0, "y": 100.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": 0 }, "size": { "width": 200, "height": 200 }, "text": "a"
            })),
            widget("sticker", 2, json!({
                "_position": { "offsetPx": { "x": 400.0, "y": 300.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": 0 }, "size": { "width": 200, "height": 200 }, "text": "b"
            })),
            json!({ "id": 3, "type": 10, "items": [1, 2] })
        ]));

        let (frame, group) = (outcome.items[0], outcome.items[3]);
        assert_eq!(board.parent_of(group), Some(frame), "the group inherits its members' frame");
        assert_eq!(board.parent_of(outcome.items[1]), Some(group));
        assert_eq!(board.parent_of(outcome.items[2]), Some(group));
        assert_eq!(board.children(Some(group)), vec![outcome.items[1], outcome.items[2]]);

        // A group has no placement of its own, so it takes its members' extent —
        // otherwise it would be a zero-size item stranded at the origin.
        let placement = board.item(group).unwrap().placement;
        assert_eq!((placement.width, placement.height), (500.0, 400.0));
    }

    /// Array order is z-order, and it has to survive both passes — including the
    /// case that breaks a naive implementation, where a parent appears *after* its
    /// children in the array.
    #[test]
    fn z_order_follows_the_clipboard_array_order() {
        let child = |id: i64, parent: i64| {
            widget("sticker", id, json!({
                "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": parent }, "size": { "width": 10, "height": 10 }, "text": "x"
            }))
        };
        let (board, outcome, _dir) = run(json!([
            child(0, 3),
            child(1, 3),
            child(2, 3),
            widget("frame", 3, json!({
                "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
                "width": 100.0, "height": 100.0, "text": "late"
            })),
            widget("sticker", 4, json!({
                "_position": { "offsetPx": { "x": 500.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
                "size": { "width": 10, "height": 10 }, "text": "top level"
            }))
        ]));

        let frame = outcome.items[3];
        assert_eq!(
            board.children(Some(frame)),
            vec![outcome.items[0], outcome.items[1], outcome.items[2]],
            "siblings must stack in the order Miro listed them"
        );
        assert_eq!(board.children(None), vec![frame, outcome.items[4]]);

        let mut z: Vec<_> = board.children(Some(frame)).iter().map(|&id| board.z_index(id)).collect();
        let sorted = { z.sort(); z };
        assert_eq!(
            sorted,
            board.children(Some(frame)).iter().map(|&id| board.z_index(id)).collect::<Vec<_>>()
        );
    }

    fn image_widget(resource: &str) -> Value {
        widget("image", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "crop": { "x": 0, "y": 0, "width": 1920, "height": 1080, "shape": "custom" },
            "resource": { "id": resource, "name": "diagram.png", "width": 1920, "height": 1080 }
        }))
    }

    /// A `.rtb`-shaped archive, so the join is exercised against the real
    /// structure rather than an idealised one.
    fn archive(assets: &[(&str, &[u8])]) -> (tempfile::TempDir, ArchiveSet) {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.rtb");
        let mut zip = std::fs::File::create(&path).map(zip::ZipWriter::new).unwrap();
        let options: zip::write::FileOptions<'_, ()> = Default::default();

        zip.start_file("board.json", options).unwrap();
        zip.write_all(br#"{"id":-1234567890123456789,"name":"Reference Board"}"#).unwrap();

        let manifest: Vec<String> = assets
            .iter()
            .map(|(id, _)| {
                format!(r#"{{"id":{id},"name":"diagram.png","extension":"png","infected":false}}"#)
            })
            .collect();
        zip.start_file("resources.json", options).unwrap();
        write!(zip, r#"{{"resources":[{}]}}"#, manifest.join(",")).unwrap();

        for (id, bytes) in assets {
            zip.start_file(format!("{id}.png"), options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        (dir, RtbArchive::open(&path).unwrap().into())
    }

    /// The join that makes an import more than an outline: `resource.id` from the
    /// clipboard, bytes from the archive, and a content hash on the item so the
    /// board file never depends on Miro's id namespace.
    #[test]
    fn image_bytes_come_from_the_rtb_and_the_item_holds_their_content_hash() {
        let pixels = b"\x89PNG\r\n\x1a\nreal bytes";
        let (_archive_dir, mut rtb) = archive(&[("3458764500000000003", pixels)]);
        let (_dir, store) = blobs();
        let mut board = Board::new();

        let outcome =
            import(&clipboard(json!([image_widget("3458764500000000003")])), Some(&mut rtb), &store, &mut board)
                .unwrap()
                .unwrap();

        let ItemKind::Image { asset_id, crop } = board.item(outcome.items[0]).unwrap().kind else {
            panic!("expected an image")
        };
        assert_eq!(asset_id, vellum_store::Hash::of(pixels).to_hex());
        assert_eq!(store.get(&asset_id.parse().unwrap()).unwrap().as_deref(), Some(&pixels[..]));
        assert_eq!(crop.unwrap().width, 1920.0);

        assert_eq!(outcome.assets_recovered, 1);
        assert_eq!(outcome.asset_bytes, pixels.len() as u64);
        assert!(outcome.missing_assets.is_empty());
        // An image carries no `size`; its extent is the crop rectangle.
        assert_eq!(board.item(outcome.items[0]).unwrap().placement.width, 1920.0);
    }

    /// The same resource on many widgets must be read, hashed and stored once —
    /// the reference archive is 109MB and a board reuses images freely.
    #[test]
    fn a_resource_shared_by_several_widgets_is_stored_once() {
        let (_archive_dir, mut rtb) = archive(&[("777", b"shared pixels")]);
        let (_dir, store) = blobs();
        let mut board = Board::new();

        let objects = json!([image_widget("777"), image_widget("777"), image_widget("777")]);
        let outcome =
            import(&clipboard(objects), Some(&mut rtb), &store, &mut board).unwrap().unwrap();

        assert_eq!(outcome.items.len(), 3);
        assert_eq!(outcome.assets_recovered, 1, "three widgets, one asset");
        assert_eq!(outcome.asset_bytes, b"shared pixels".len() as u64);
    }

    /// The failure mode this project cares about most: content that is missing but
    /// looks present. A referenced-but-absent asset must still produce an item, at
    /// the right place, and must be named in the report.
    #[test]
    fn assets_that_are_referenced_but_absent_are_reported_not_dropped() {
        let (_archive_dir, mut rtb) = archive(&[("111", b"some other image")]);
        let (_dir, store) = blobs();
        let mut board = Board::new();

        let outcome =
            import(&clipboard(json!([image_widget("999")])), Some(&mut rtb), &store, &mut board)
                .unwrap()
                .unwrap();

        assert_eq!(board.item_count(), 1, "the item must still exist");
        assert_eq!(outcome.missing_assets.len(), 1);
        let missing = &outcome.missing_assets[0];
        assert_eq!(missing.resource_id, "999");
        assert_eq!(missing.name.as_deref(), Some("diagram.png"));
        assert_eq!(missing.gap, AssetGap::NotInArchive);
        assert_eq!(missing.item, outcome.items[0]);

        assert!(outcome.report.missing_assets[0].contains("999"), "{:?}", outcome.report.missing_assets);
        assert!(outcome.to_string().contains("missing asset"), "{outcome}");
    }

    #[test]
    fn without_an_archive_every_asset_is_a_reported_gap() {
        let (_board, outcome, _dir) = run(json!([image_widget("42")]));
        assert_eq!(outcome.missing_assets[0].gap, AssetGap::NoArchive);
        assert_eq!(outcome.assets_recovered, 0);
    }

    /// A connector's whole value is that it binds to widgets. Vellum has no
    /// connector kind, so the binding is what is lost — but the *line* must still
    /// be drawn between the bounds it was bound to, not collapsed to the origin.
    #[test]
    fn connector_geometry_is_derived_from_the_widgets_it_binds_to() {
        let box_at = |id: i64, x: f64| {
            widget("sticker", id, json!({
                "_position": { "offsetPx": { "x": x, "y": 0.0 }, "schema": "canvasOffsetPx" },
                "size": { "width": 100, "height": 40 }, "text": "x"
            }))
        };
        let (board, outcome, _dir) = run(json!([
            box_at(0, 0.0),
            box_at(1, 500.0),
            widget("line", 2, json!({
                "points": [], "_position": null,
                "primary": { "point": { "x": 1, "y": 0.5 }, "widgetIndex": 0 },
                "secondary": { "point": { "x": 0, "y": 0.5 }, "widgetIndex": 1 },
                "style": "{\"lc\":3355443,\"t\":2,\"a_end\":9}"
            }))
        ]));

        let item = board.item(outcome.items[2]).unwrap();
        let ItemKind::Ink { points, color, thickness } = item.kind else { panic!("expected ink") };
        // Right edge of the first box (50, 0) to the left edge of the second (450, 0).
        assert_eq!((item.placement.x, item.placement.y), (250.0, 0.0));
        assert_eq!(item.placement.width, 400.0);
        assert_eq!(points, vec![Point::new(-200.0, 0.0), Point::new(200.0, 0.0)]);
        assert_eq!(color.unwrap().to_hex(), "#333333");
        assert_eq!(thickness, 2.0);

        let (degradation, n) =
            outcome.degraded.iter().find(|(d, _)| d.miro_type == "connector").unwrap();
        assert_eq!(*n, 1);
        assert!(degradation.lost.contains("re-route"), "{}", degradation.lost);
    }

    /// Ink is what makes the clipboard route worth having at all — no Miro API
    /// exposes drawings. Points arrive relative to the stroke's own corner and must
    /// end up relative to its centre.
    #[test]
    fn ink_points_are_recentred_and_opacity_folds_into_the_colour() {
        let (board, outcome, _dir) = run(json!([widget("paint", 0, json!({
            "_position": { "offsetPx": { "x": -3351.5, "y": -575.4 }, "schema": "canvasOffsetPx" },
            "points": [{ "x": 0, "y": 0 }, { "x": 10.0, "y": 4.0 }],
            "style": "{\"lc\":3000156,\"t\":18,\"lo\":0.5}"
        }))]));

        let item = board.item(outcome.items[0]).unwrap();
        let ItemKind::Ink { points, color, thickness } = item.kind else { panic!("expected ink") };
        assert_eq!(points, vec![Point::new(-5.0, -2.0), Point::new(5.0, 2.0)]);
        assert_eq!((item.placement.width, item.placement.height), (10.0, 4.0));
        assert_eq!(thickness, 18.0);
        let color = color.unwrap();
        assert_eq!(color.to_hex(), "#2dc75c80");
        assert_eq!(color.a, 128, "Miro's separate `lo` must fold into alpha");
    }

    #[test]
    fn rich_document_deltas_become_styled_spans() {
        let (board, outcome, _dir) = run(json!([widget("structured_document", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "content": [
                { "insert": "MaxxECU", "attributes": { "bold": true } },
                { "insert": " wiring", "attributes": { "color": "#3578ff" } },
                { "insert": "https://example.com",
                  "attributes": { "link": "https://example.com", "underline": true } }
            ]
        }))]));

        let ItemKind::Text { text } = board.item(outcome.items[0]).unwrap().kind else {
            panic!("expected text")
        };
        let spans = text.spans();
        assert_eq!(spans.len(), 3);
        assert!(spans[0].style.bold);
        assert_eq!(spans[1].style.color.unwrap().to_hex(), "#3578ff");
        assert_eq!(spans[2].style.link.as_deref(), Some("https://example.com"));
        assert!(spans[2].style.underline);
    }

    /// Real text from the board: nested formatting, an `<a href>` whose query
    /// string is full of escaped `=`, and Quill's list markup.
    #[test]
    fn html_formatting_links_and_entities_all_survive() {
        let html = "<p>A 2020 unit runs the <strong>MK4 controller</strong>, \
                    not the older MK3 Evo.</p>\
                    <ol><li data-list=\"bullet\"><span class=\"ql-ui\"></span>Wider bracket 1k</li></ol>\
                    <p><a href=\"https://x.test/p?c&#61;1&amp;d&#61;2\">link &#34;here&#34;</a></p>\
                    <p><br /></p>";
        let (board, outcome, _dir) = run(json!([widget("text", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 400, "height": 200 }, "text": html,
            "style": "{\"ffn\":\"Noto Sans\",\"fs\":14,\"ta\":\"l\",\"tc\":1710618}"
        }))]));

        let item = board.item(outcome.items[0]).unwrap();
        let ItemKind::Text { text } = &item.kind else { panic!("expected text") };
        assert_eq!(
            text.to_plain(),
            "A 2020 unit runs the MK4 controller, not the older MK3 Evo.\n\
             Wider bracket 1k\n\
             link \"here\"",
            "trailing empty paragraphs are editor artefacts, not content"
        );

        let bold = text.spans().iter().find(|s| s.style.bold).expect("<strong> must survive");
        assert_eq!(bold.text, "MK4 controller");
        let link = text.spans().iter().find(|s| s.style.link.is_some()).expect("<a> must survive");
        assert_eq!(link.style.link.as_deref(), Some("https://x.test/p?c=1&d=2"));

        // Widget-level typography belongs to the item, not to its spans.
        assert_eq!(item.style.font_family.as_deref(), Some("Noto Sans"));
        assert_eq!(item.style.font_size, Some(14.0));
        assert_eq!(item.style.align, Some(Align::Left));
        assert_eq!(item.style.text_color.unwrap().to_hex(), "#1a1a1a");

        // And the list structure that a flat span model cannot carry is declared.
        let (degradation, n) =
            outcome.degraded.iter().find(|(d, _)| d.miro_type.contains("list")).unwrap();
        assert_eq!(*n, 1);
        assert!(degradation.lost.contains("list model"), "{}", degradation.lost);
    }

    /// A `preview` becomes a **card**, not a paragraph.
    ///
    /// It used to become `ItemKind::Text` holding the title, URL and description as three
    /// styled lines — which is why 132 of the reference board's 596 widgets drew as grey text
    /// while `ItemKind::LinkPreview` and its card painter sat unused. The provider is derived
    /// from the host here, with no network, so the card names its site immediately.
    #[test]
    fn a_link_preview_becomes_a_card_that_names_its_site() {
        let (board, outcome, _dir) = run(json!([widget("preview", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 250, "height": 190 },
            "openGraph": { "title": "Acme F90 M5 Shifter", "description": "8HP shifter",
                           "url": "https://wiki.example.net/x" }
        }))]));

        let ItemKind::LinkPreview { title, url, description, provider, thumbnail, mode, .. } =
            board.item(outcome.items[0]).unwrap().kind
        else {
            panic!("expected a link preview card")
        };
        assert_eq!(title.as_deref(), Some("Acme F90 M5 Shifter"));
        assert_eq!(url.as_deref(), Some("https://wiki.example.net/x"));
        assert_eq!(description.as_deref(), Some("8HP shifter"));
        assert_eq!(provider.as_deref(), Some("Example"), "named from the host, offline");
        assert_eq!(thumbnail, None, "nothing is fetched during an import");
        assert_eq!(mode, vellum_doc::CardMode::Card);

        // And it is no longer reported as lost.
        assert!(
            !outcome.degraded.iter().any(|(d, _)| d.miro_type == "link_preview"),
            "a card is not a degradation: {:?}",
            outcome.degraded
        );
    }

    /// A card's preview picture comes out of the `.rtb` like any other asset.
    ///
    /// Miro fetches a page's OpenGraph image once, stores it as a board resource, and points
    /// the widget at it through `resourceWidget`. Taking that is what makes an imported card
    /// look like the one on the board **without a single request** — which matters because
    /// re-fetching largely cannot work: measured against the reference board's own links,
    /// Amazon answers 404 to a non-browser agent, eBay 403, and Alibaba a page with no
    /// OpenGraph tags. Those three are 49 of its 131 links.
    #[test]
    fn a_cards_preview_image_comes_from_the_archive_with_no_fetch() {
        let pixels = b"\x89PNG\r\n\x1a\nposter frame";
        let (_archive_dir, mut rtb) = archive(&[("3458764500000000004", pixels)]);
        let (_dir, store) = blobs();
        let mut board = Board::new();

        let objects = json!([widget("preview", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 250, "height": 361 },
            "openGraph": { "title": "Acme Radiator", "url": "https://parts.example.com/x" },
            // The form Miro actually writes: the id and the dimensions live under `meta`,
            // not beside `id` as they do for an image widget's `resource`.
            "visualType": 2,
            "resourceWidget": {
                "id": "3458764500000000004",
                "name": "MAH-CR923000P.JPG",
                "meta": {
                    "extension": "jpg",
                    "externalLink": "https://parts.example.com/public/assets/photo.JPG",
                    "width": 900, "height": 900
                }
            }
        }))]);
        let outcome =
            import(&clipboard(objects), Some(&mut rtb), &store, &mut board).unwrap().unwrap();

        let ItemKind::LinkPreview { thumbnail, mode, .. } =
            board.item(outcome.items[0]).unwrap().kind
        else {
            panic!("expected a link preview card")
        };
        let hash = thumbnail.expect("the card takes Miro's stored preview image");
        assert_eq!(
            store.get(&hash.parse().unwrap()).unwrap().as_deref(),
            Some(&pixels[..]),
            "and it is the archive's bytes, addressed by content"
        );
        assert_eq!(mode, CardMode::Large, "`visualType: 2` is Miro's large preview");

        // A card image is not an image *widget*, so it must not be counted as one.
        assert!(
            outcome.missing_assets.is_empty(),
            "a resolved card image is no kind of gap: {:?}",
            outcome.missing_assets
        );
    }

    /// A card whose stored resource is gone is still a card, and is **not** a missing asset.
    ///
    /// Miro prunes a preview image once the board stops showing it — 7 of the reference
    /// board's 91 previews and 15 of its 41 embeds point at a resource the `.rtb` no longer
    /// carries. Reporting those as missing assets would put "22 without their assets" in the
    /// paste toast for cards that are complete in every way a user can see.
    #[test]
    fn a_pruned_card_image_is_not_reported_as_a_missing_asset() {
        let (_archive_dir, mut rtb) = archive(&[("3458764500000000004", b"other")]);
        let (_dir, store) = blobs();
        let mut board = Board::new();

        let objects = json!([widget("preview", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 250, "height": 289 },
            "openGraph": { "title": "Gone", "url": "https://example.com/x" },
            "resourceWidget": { "id": "9999999999999999999", "meta": {} }
        }))]);
        let outcome =
            import(&clipboard(objects), Some(&mut rtb), &store, &mut board).unwrap().unwrap();

        let ItemKind::LinkPreview { thumbnail, title, .. } =
            board.item(outcome.items[0]).unwrap().kind
        else {
            panic!("expected a link preview card")
        };
        assert_eq!(thumbnail, None);
        assert_eq!(title.as_deref(), Some("Gone"), "the card is otherwise whole");
        assert!(outcome.missing_assets.is_empty(), "{:?}", outcome.missing_assets);
    }

    /// An `embed` becomes the embed card, keeping the provider its URL implies.
    #[test]
    fn an_embed_becomes_a_card_with_its_provider() {
        let (board, outcome, _dir) = run(json!([widget("embed", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 400, "height": 260 },
            "custom_data": { "title": "Finally Driving The Acme M5 F90",
                             "url": "https://www.youtube.com/watch?v=aqz-KE-bpKQ" }
        }))]));

        let ItemKind::Embed { title, url, provider, thumbnail, .. } =
            board.item(outcome.items[0]).unwrap().kind
        else {
            panic!("expected an embed card")
        };
        assert_eq!(title.as_deref(), Some("Finally Driving The Acme M5 F90"));
        assert_eq!(url.as_deref(), Some("https://www.youtube.com/watch?v=aqz-KE-bpKQ"));
        assert_eq!(provider.as_deref(), Some("YouTube"));
        assert_eq!(thumbnail, None);

        // Still reported — the live frame is genuinely not imported — but as an embed rather
        // than as text, and the wording says what a card does instead.
        let (degradation, n) =
            outcome.degraded.iter().find(|(d, _)| d.miro_type == "embed").expect("still reported");
        assert_eq!(*n, 1);
        assert_eq!(degradation.imported_as, "embed");
        assert!(degradation.lost.contains("live frame"), "{}", degradation.lost);
    }

    /// An embed keeps the three things Miro already knows and the decoder used to drop:
    /// **its own name for the site**, the provider's iframe markup, and the poster frame.
    ///
    /// `provider.name` beats the host-derived guess because it is what the board shows. The
    /// two agree on "YouTube" and diverge on the sites a bare host is least sure about — the
    /// reference board's embeds name Partsdb, Instrutec and Behance this way.
    #[test]
    fn an_embed_keeps_miros_provider_name_its_iframe_and_its_poster() {
        let poster = b"\x89PNG\r\n\x1a\nhqdefault";
        let (_archive_dir, mut rtb) = archive(&[("3458764500000000005", poster)]);
        let (_dir, store) = blobs();
        let mut board = Board::new();

        let objects = json!([widget("embed", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 400, "height": 260 },
            "provider": { "name": "Partsdb", "iconUrl": "", "url": "" },
            "custom_data": {
                "title": "Acme parts diagram",
                "url": "https://parts.example.com/enUS/part",
                "provider_name": "partsdb.example",
                "html": "<iframe src=\"//cdn.embedly.com/x\"></iframe>"
            },
            "resourceWidget": {
                "id": "3458764500000000005",
                "meta": { "externalLink": "https://i.ytimg.com/vi/x/hqdefault.jpg" }
            }
        }))]);
        let outcome =
            import(&clipboard(objects), Some(&mut rtb), &store, &mut board).unwrap().unwrap();

        let ItemKind::Embed { provider, html, thumbnail, .. } =
            board.item(outcome.items[0]).unwrap().kind
        else {
            panic!("expected an embed card")
        };
        assert_eq!(
            provider.as_deref(),
            Some("Partsdb"),
            "the widget's own provider object wins over `custom_data`'s flat name and over the host"
        );
        assert!(
            html.expect("the iframe markup is kept").contains("embedly"),
            "kept verbatim so the no-webview decision stays reversible without a re-import"
        );
        let hash = thumbnail.expect("the poster frame joins from the archive");
        assert_eq!(store.get(&hash.parse().unwrap()).unwrap().as_deref(), Some(&poster[..]));
    }

    /// A type Miro has not shown us yet must be counted and named, never dropped —
    /// the report is what tells a user their board is not all here.
    #[test]
    fn unmapped_types_are_placed_counted_and_named() {
        let (board, outcome, _dir) = run(json!([widget("mindmap_node", 0, json!({
            "_position": { "offsetPx": { "x": 7.0, "y": 9.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 40, "height": 20 }
        }))]));

        assert_eq!(board.item_count(), 1);
        let placement = board.item(outcome.items[0]).unwrap().placement;
        assert_eq!((placement.x, placement.y, placement.width), (7.0, 9.0, 40.0));
        assert_eq!(outcome.unmapped_types.get("mindmap_node"), Some(&1));
        assert!(outcome.to_string().contains("mindmap_node"), "{outcome}");
    }

    /// A paste is user-triggered and must never hang or take the app down, however
    /// malformed the payload.
    #[test]
    fn a_parent_cycle_is_broken_and_reported_rather_than_hanging() {
        let cyclic = |id: i64, parent: i64| {
            widget("sticker", id, json!({
                "_position": { "offsetPx": { "x": 1.0, "y": 1.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": parent }, "size": { "width": 10, "height": 10 }, "text": "x"
            }))
        };
        let (board, outcome, _dir) = run(json!([cyclic(0, 1), cyclic(1, 2), cyclic(2, 0)]));

        assert_eq!(board.item_count(), 3, "nothing is lost to a malformed hierarchy");
        assert_eq!(board.children(None).len(), 1, "the cycle is cut in exactly one place");
        assert!(
            outcome.report.warnings.iter().any(|w| w.contains("own ancestor")),
            "{:?}",
            outcome.report.warnings
        );
    }

    #[test]
    fn non_miro_html_is_not_an_import_and_not_an_error() {
        let (_dir, store) = blobs();
        let mut board = Board::new();
        assert!(import("<p>hello</p>", None, &store, &mut board).unwrap().is_none());
        assert!(board.is_empty());
    }

    /// The whole paste is one undo step. Six hundred items appearing and then
    /// needing six hundred undos would make an accidental paste unrecoverable.
    #[test]
    fn an_import_undoes_as_a_single_step() {
        let (mut board, _outcome, _dir) = run(frame_with_child());
        assert_eq!(board.item_count(), 2);

        assert!(board.undo().unwrap());
        assert!(board.is_empty(), "one undo must remove the entire paste");
    }

    /// The import writes into a real document, so it must round-trip through the
    /// on-disk format with its hierarchy and content intact.
    #[test]
    fn an_imported_board_survives_a_save_and_reload() {
        let (board, outcome, _dir) = run(frame_with_child());
        let reopened = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();

        assert_eq!(reopened.item_count(), 2);
        assert_eq!(reopened.parent_of(outcome.items[1]), Some(outcome.items[0]));
        let ItemKind::Sticky { text, .. } = reopened.item(outcome.items[1]).unwrap().kind else {
            panic!("expected a sticky")
        };
        assert_eq!(text.to_plain(), "fan");
    }

    /// Pasting onto a board that already has content stacks on top of it rather
    /// than underneath, and leaves what was there alone.
    #[test]
    fn a_paste_stacks_on_top_of_whatever_is_already_on_the_board() {
        let (_dir, store) = blobs();
        let mut board = Board::new();
        let existing = board
            .add(NewItem::new(
                ItemKind::Text { text: StyledText::plain("mine") },
                Placement::new(0.0, 0.0, 10.0, 10.0),
            ))
            .unwrap();

        let outcome =
            import(&clipboard(frame_with_child()), None, &store, &mut board).unwrap().unwrap();

        assert_eq!(board.children(None), vec![existing, outcome.items[0]]);
        assert_eq!(board.item_count(), 3);
    }

    #[test]
    fn the_oracle_reports_disagreements_and_folds_them_into_the_report() {
        let svg = tempfile::NamedTempFile::new().unwrap();
        // Two stickies in the export against one in the import.
        std::fs::write(
            svg.path(),
            r##"<svg width="10px" height="10px">
                  <use xlink:href="#StickerType1" fill="#fff79e"/>
                  <use xlink:href="#StickerType1" fill="#fff79e"/>
                </svg>"##,
        )
        .unwrap();

        let (_board, mut outcome, _dir) = run(frame_with_child());
        let discrepancies = outcome.cross_check(svg.path()).unwrap().to_vec();

        let sticky = discrepancies.iter().find(|d| d.what == "sticky").expect("a shortfall");
        assert_eq!((sticky.svg_says, sticky.import_says), (2, 1));
        assert!(
            outcome.report.warnings.iter().any(|w| w.contains("oracle")),
            "a discrepancy must reach the user, not just the caller"
        );
        assert!(outcome.to_string().contains("oracle"), "{outcome}");
    }

    #[test]
    fn the_summary_says_what_arrived_and_what_it_cost() {
        let (_board, outcome, _dir) = run(frame_with_child());
        let summary = outcome.to_string();

        assert!(summary.starts_with("2 items imported from Miro (board bTBja0JvYXJkSWQ=)"), "{summary}");
        assert!(summary.contains("1  sticky"), "{summary}");
        // A frame is no longer a substitution, so it must not be reported as one.
        assert!(!summary.contains("frame → text"), "{summary}");
        assert_eq!(outcome.total(), 2);
        assert_eq!(outcome.lossless(), 2, "a frame now imports as a frame, losing nothing");
    }

    /// Entities appear inside link query strings on the real board, where getting
    /// them wrong silently breaks the URL.
    #[test]
    fn entity_decoding_covers_the_forms_miro_emits() {
        let decode = richtext::decode_entities;
        assert_eq!(decode("a&#61;1&amp;b&#43;2"), "a=1&b+2");
        assert_eq!(decode("&#34;quoted&#34; &#39;and&#39;"), "\"quoted\" 'and'");
        assert_eq!(decode("&lt;p&gt;"), "<p>");
        assert_eq!(decode("&#x41;&#x42;"), "AB");
        assert_eq!(decode("no entities here"), "no entities here");
        assert_eq!(decode("bare & ampersand"), "bare & ampersand");
        assert_eq!(decode("&notanentity;"), "&notanentity;");
    }

    /// The tag walk is recursive and the clipboard is untrusted, so nesting deep
    /// enough to overflow the stack must degrade to plain text instead.
    #[test]
    fn pathological_nesting_keeps_the_text_and_drops_the_markup() {
        let html = format!("{}deep{}", "<div>".repeat(5_000), "</div>".repeat(5_000));
        let (board, outcome, _dir) = run(json!([widget("text", 0, json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "size": { "width": 10, "height": 10 }, "text": html
        }))]));

        let ItemKind::Text { text } = board.item(outcome.items[0]).unwrap().kind else {
            panic!("expected text")
        };
        assert_eq!(text.to_plain(), "deep");
    }

    #[test]
    fn hex_colours_accept_both_css_forms() {
        assert_eq!(richtext::hex_color("#3578ff").unwrap().to_hex(), "#3578ff");
        assert_eq!(richtext::hex_color("#fff").unwrap().to_hex(), "#ffffff");
        assert_eq!(richtext::hex_color("blue"), None);
        assert_eq!(richtext::hex_color("#12345"), None);
    }

    /// Every entry in the table must name a type the mapper can actually produce,
    /// or the report will promise an explanation that never appears.
    #[test]
    fn every_declared_degradation_names_a_real_widget_type() {
        // Deliberately no `Frame`: it converts to `ItemKind::Frame` exactly now, keeping
        // its background, presentation order and speaker notes, so it costs nothing and
        // must not claim a row.
        let produced = [
            WidgetKind::Group { children: Vec::new() },
            WidgetKind::Connector {
                start: miro::ConnectorEnd { target: None, anchor: (0.0, 0.0), arrowhead: 0 },
                end: miro::ConnectorEnd { target: None, anchor: (0.0, 0.0), arrowhead: 0 },
                style: miro::ConnectorStyle::default(),
                captions: Vec::new(),
            },
            // `LinkPreview` is deliberately absent: it imports as a card now, losing
            // nothing, so it has no row in `DEGRADATIONS` for this to match. `Embed` keeps
            // one, because the live frame really is not imported.
            WidgetKind::Embed {
                title: None,
                url: None,
                description: None,
                provider: None,
                html: None,
                image: None,
            },
            WidgetKind::Document {
                asset: miro::AssetRef {
                    id: String::new(),
                    name: None,
                    width: None,
                    height: None,
                    external_url: None,
                },
            },
            WidgetKind::RichDocument { ops: Vec::new() },
        ];
        let labels: Vec<&str> = produced.iter().map(WidgetKind::label).collect();
        for degradation in DEGRADATIONS {
            assert!(
                labels.contains(&degradation.miro_type),
                "`{}` is not a WidgetKind label",
                degradation.miro_type
            );
        }
        assert_eq!(DEGRADATIONS.len(), labels.len(), "a degraded type is undeclared");
    }

    /// The board's name lives only in the archive — the clipboard payload carries
    /// an id and no title — so importing into a fresh board has to reach for it.
    #[test]
    fn a_new_board_takes_its_name_from_the_archive() {
        let (_archive_dir, mut rtb) = archive(&[("1", b"x")]);
        let (_dir, store) = blobs();

        let (board, outcome) =
            import_to_new_board(&clipboard(frame_with_child()), Some(&mut rtb), &store)
                .unwrap()
                .unwrap();

        assert_eq!(board.title(), "Reference Board");
        assert_eq!(board.item_count(), 2);
        assert_eq!(outcome.total(), 2);

        assert!(import_to_new_board("<p>not miro</p>", None, &store).unwrap().is_none());
    }

    /// A payload with nothing in it is a legitimate copy of an empty selection.
    #[test]
    fn an_empty_payload_imports_as_nothing_without_complaining() {
        let (board, outcome, _dir) = run(json!([]));
        assert!(board.is_empty());
        assert_eq!(outcome.total(), 0);
        assert!(outcome.report.warnings.is_empty());
    }
}
