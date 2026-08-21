//! The icon set: geometry in the source, no asset files.
//!
//! Every icon is a handful of polylines and circles in a unit box, painted with
//! `epaint` primitives at whatever size the caller asks for. That is deliberate:
//!
//! - **No asset files and no icon font.** A bundled SVG set would need a rasteriser
//!   and a cache; an icon font would need a second font in the atlas and would still
//!   render as tofu the moment a glyph is missing. Both cost more than the geometry
//!   below.
//! - **Resolution independence for free.** The same table draws a 16px menu icon and
//!   a 40px tool button on a Retina display with no mip levels and no blur.
//!
//! # Where the geometry comes from
//!
//! Most of it was drawn by hand, point by point. That works for a rectangle and fails
//! badly for anything organic: the **hand** took three hand-drawn attempts — an outline,
//! a filled silhouette, and strokes — and all three read as a smudge at the 24pt the
//! palette actually draws, because finger gaps of 0.02–0.04 of a unit box are sub-pixel
//! there. Each attempt had to be screenshotted to find that out.
//!
//! The organic ones are therefore **converted from [Lucide](https://lucide.dev)** by
//! `scripts/icons.py`, which flattens its SVG paths — béziers and elliptical arcs — into
//! the polylines `Prim` can hold. Lucide draws on a 24×24 grid with round caps, which is
//! the model `Icon::paint` already had, so the two meet without adapting either.
//!
//! Regenerate one with, for example:
//!
//! ```text
//! python3 scripts/icons.py --map Hand=hand --map Pen=pencil
//! ```
//!
//! ## Licence
//!
//! Lucide is ISC, and the notice travels with the geometry:
//!
//! > ISC License
//! >
//! > Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of
//! > Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors
//! > 2022.
//! >
//! > Permission to use, copy, modify, and/or distribute this software for any purpose
//! > with or without fee is hereby granted, provided that the above copyright notice
//! > and this permission notice appear in all copies.
//! >
//! > THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
//! > REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND
//! > FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT,
//! > OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE,
//! > DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS
//! > ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS
//! > SOFTWARE.
//!
//! The unit box matches `vellum-shapes`: `x` and `y` both run `0..1` with y
//! downwards.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, epaint::PathShape};

/// One stroke or fill inside an icon's unit box.
#[derive(Debug, Clone, Copy)]
pub enum Prim {
    Poly { pts: &'static [(f32, f32)], closed: bool, fill: bool },
    Circle { c: (f32, f32), r: f32, fill: bool },
}

const fn line(pts: &'static [(f32, f32)]) -> Prim {
    Prim::Poly { pts, closed: false, fill: false }
}

const fn outline(pts: &'static [(f32, f32)]) -> Prim {
    Prim::Poly { pts, closed: true, fill: false }
}

const fn solid(pts: &'static [(f32, f32)]) -> Prim {
    Prim::Poly { pts, closed: true, fill: true }
}

/// A rectangle given by two opposite corners, as a closed contour.
macro_rules! rect_pts {
    ($x0:expr, $y0:expr, $x1:expr, $y1:expr) => {
        &[($x0, $y0), ($x1, $y0), ($x1, $y1), ($x0, $y1)]
    };
}

/// Every icon the chrome draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    // Tools
    Select,
    Hand,
    Sticky,
    Text,
    Shape,
    Pen,
    Eraser,
    Connector,
    Frame,
    Table,
    Chart,
    MindMap,
    Kanban,
    Image,
    // Z-order
    BringToFront,
    BringForward,
    SendBackward,
    SendToBack,
    // Structure
    Group,
    Ungroup,
    Lock,
    Unlock,
    // Align and distribute
    AlignLeft,
    AlignCenterHorizontal,
    AlignRight,
    AlignTop,
    AlignMiddleVertical,
    AlignBottom,
    DistributeHorizontal,
    DistributeVertical,
    // History
    Undo,
    Redo,
    // View
    ZoomIn,
    ZoomOut,
    ZoomToFit,
    Minimap,
    Grid,
    /// Stroke thickness: three strokes, thin to thick.
    ///
    /// The bar showed a bare `4.0` beside a bare `100%` with nothing saying which was
    /// which — *"for the pen small menu please indicate what each number does with an
    /// icon"*. A ramp is the one drawing that means "how thick" without a word, and it is
    /// what every drawing tool uses. The thickening is done with three separate lines
    /// rather than a wedge because a wedge at 16pt reads as a triangle.
    Thickness,
    /// Opacity: a circle half-filled with the checkerboard that means "transparent".
    Opacity,
    Present,
    // Text
    TextAlignLeft,
    TextAlignCenter,
    TextAlignRight,
    // Library
    Star,
    StarFilled,
    Pin,
    More,
    List,
    /// Three plain rules — the ☰ that opens the board menu from the top-left pill.
    ///
    /// Distinct from [`Icon::List`], which is dots *and* rules and therefore reads as a
    /// list of things rather than as a menu.
    Hamburger,
    Upload,
    // General
    Search,
    Plus,
    Close,
    Check,
    ChevronUp,
    ChevronDown,
    ChevronRight,
    /// The mirror of [`Self::ChevronRight`], for a control that points the other way — the
    /// arrowhead at a connector's *start*, which on an agent link is the direction messages
    /// travel in.
    ChevronLeft,
    Import,
    Duplicate,
    Trash,
    Info,
    Sun,
    Moon,
    /// A folder, for the board library's filing.
    Folder,
    // The Agent Canvas layer's four node types. Each had to be legible at 16pt and
    // distinguishable from every icon already here, which is what ruled out the obvious
    // drawings: a plain rounded box is the shape tool, a folded corner is a sticky, and a
    // rectangle with content in it is the image.
    /// An agent: a terminal prompt — a chevron and a caret rule — inside a box. Says
    /// "something is running in here" rather than "this is a rectangle".
    Agent,
    /// A note: a page with a folded top-right corner and two written lines. The fold is
    /// the opposite corner from [`Icon::Sticky`]'s, which is what tells the two apart at
    /// palette size.
    Note,
    /// A file tree: a root, a stem, and two branches ending in rows.
    FileTree,
    /// A browser: a frame with a chrome bar and two dots in it.
    Browser,
    /// Start — a filled triangle, drawn rather than the character `▶`.
    ///
    /// U+25B6 is outside every plain sans face this app ships or falls back to (trap 10),
    /// so the character would draw tofu on the one button whose job is to be unmistakable.
    Play,
    /// Stop — a filled square, which is what every transport control in existence uses and
    /// the only shape that cannot be mistaken for *close* the way an ✕ can.
    Stop,
}

impl Icon {
    /// The icon's geometry in the unit box.
    #[expect(clippy::too_many_lines, reason = "a lookup table reads better unsplit")]
    const fn prims(self) -> &'static [Prim] {
        match self {
            Self::Select => const { &[
                outline(&[(0.168, 0.195), (0.168, 0.179), (0.179, 0.168), (0.195, 0.168), (0.862, 0.439), (0.874, 0.451), (0.873, 0.468), (0.859, 0.478), (0.604, 0.544), (0.584, 0.553), (0.566, 0.566), (0.553, 0.584), (0.544, 0.604), (0.478, 0.859), (0.468, 0.873), (0.451, 0.874), (0.439, 0.862)]),
            ] },
            Self::Hand => const { &[
                line(&[(0.750, 0.458), (0.750, 0.250), (0.747, 0.228), (0.739, 0.208), (0.726, 0.191), (0.708, 0.178), (0.688, 0.170), (0.667, 0.167), (0.645, 0.170), (0.625, 0.178), (0.608, 0.191), (0.594, 0.208), (0.586, 0.228), (0.583, 0.250)]),
                line(&[(0.583, 0.417), (0.583, 0.167), (0.580, 0.145), (0.572, 0.125), (0.559, 0.108), (0.542, 0.094), (0.522, 0.086), (0.500, 0.083), (0.478, 0.086), (0.458, 0.094), (0.441, 0.108), (0.428, 0.125), (0.420, 0.145), (0.417, 0.167), (0.417, 0.250)]),
                line(&[(0.417, 0.438), (0.417, 0.250), (0.414, 0.228), (0.406, 0.208), (0.392, 0.191), (0.375, 0.178), (0.355, 0.170), (0.333, 0.167), (0.312, 0.170), (0.292, 0.178), (0.274, 0.191), (0.261, 0.208), (0.253, 0.228), (0.250, 0.250), (0.250, 0.583)]),
                line(&[(0.750, 0.333), (0.754, 0.308), (0.766, 0.284), (0.784, 0.266), (0.808, 0.254), (0.833, 0.250), (0.859, 0.254), (0.882, 0.266), (0.901, 0.284), (0.913, 0.308), (0.917, 0.333), (0.917, 0.583), (0.915, 0.612), (0.912, 0.641), (0.905, 0.670), (0.897, 0.697), (0.885, 0.724), (0.872, 0.750), (0.856, 0.775), (0.839, 0.798), (0.819, 0.819), (0.798, 0.839), (0.775, 0.856), (0.750, 0.872), (0.724, 0.885), (0.697, 0.897), (0.670, 0.905), (0.641, 0.912), (0.612, 0.915), (0.583, 0.917), (0.500, 0.917), (0.477, 0.916), (0.456, 0.915), (0.435, 0.912), (0.416, 0.909), (0.397, 0.905), (0.380, 0.900), (0.363, 0.894), (0.347, 0.888), (0.331, 0.880), (0.317, 0.872), (0.303, 0.863), (0.289, 0.853), (0.276, 0.842), (0.263, 0.831), (0.250, 0.819), (0.100, 0.669), (0.087, 0.648), (0.080, 0.624), (0.080, 0.599), (0.088, 0.575), (0.103, 0.554), (0.124, 0.539), (0.148, 0.531), (0.173, 0.531), (0.197, 0.538), (0.218, 0.552), (0.292, 0.625)]),
            ] },
            Self::Sticky => const { &[
                outline(&[(0.18, 0.18), (0.82, 0.18), (0.82, 0.60), (0.60, 0.82), (0.18, 0.82)]),
                line(&[(0.82, 0.60), (0.60, 0.60), (0.60, 0.82)]),
            ] },
            Self::Text => const { &[
                line(&[(0.24, 0.32), (0.24, 0.22), (0.76, 0.22), (0.76, 0.32)]),
                line(&[(0.50, 0.22), (0.50, 0.78)]),
                line(&[(0.38, 0.78), (0.62, 0.78)]),
            ] },
            Self::Shape => const { &[
                outline(rect_pts!(0.14, 0.32, 0.56, 0.74)),
                Prim::Circle { c: (0.66, 0.42), r: 0.24, fill: false },
            ] },
            Self::Pen => const { &[
                outline(&[(0.882, 0.284), (0.900, 0.262), (0.911, 0.236), (0.916, 0.208), (0.915, 0.180), (0.906, 0.153), (0.892, 0.128), (0.872, 0.108), (0.847, 0.094), (0.820, 0.085), (0.792, 0.084), (0.764, 0.089), (0.738, 0.100), (0.716, 0.118), (0.160, 0.674), (0.151, 0.684), (0.144, 0.696), (0.139, 0.708), (0.084, 0.890), (0.085, 0.904), (0.096, 0.915), (0.110, 0.916), (0.292, 0.861), (0.304, 0.856), (0.316, 0.849), (0.326, 0.840)]),
                line(&[(0.625, 0.208), (0.792, 0.375)]),
            ] },
            Self::Eraser => const { &[
                outline(&[(0.24, 0.60), (0.54, 0.22), (0.78, 0.42), (0.48, 0.80)]),
                line(&[(0.20, 0.84), (0.84, 0.84)]),
            ] },
            Self::Connector => const { &[
                line(&[(0.30, 0.70), (0.70, 0.30)]),
                Prim::Circle { c: (0.24, 0.76), r: 0.10, fill: false },
                Prim::Circle { c: (0.76, 0.24), r: 0.10, fill: false },
            ] },
            // A rectangle with a title tab, which is what a frame *is*. It used to
            // be a bare crosshatch, one box away from `Grid` — the two were near
            // duplicates with their mnemonics swapped.
            Self::Frame => const { &[
                line(&[(0.917, 0.250), (0.083, 0.250)]),
                line(&[(0.917, 0.750), (0.083, 0.750)]),
                line(&[(0.250, 0.083), (0.250, 0.917)]),
                line(&[(0.750, 0.083), (0.750, 0.917)]),
            ] },
            // A grid, with the header row weighted: it is what distinguishes a table
            // from the frame icon directly above it in the palette.
            Self::Table => const { &[
                outline(rect_pts!(0.14, 0.18, 0.86, 0.82)),
                line(&[(0.14, 0.36), (0.86, 0.36)]),
                line(&[(0.14, 0.59), (0.86, 0.59)]),
                line(&[(0.38, 0.18), (0.38, 0.82)]),
                line(&[(0.62, 0.18), (0.62, 0.82)]),
            ] },
            // Three bars of different heights on a baseline: the one chart form that
            // is unmistakable at 16px.
            Self::Chart => const { &[
                line(&[(0.16, 0.82), (0.84, 0.82)]),
                line(&[(0.28, 0.82), (0.28, 0.52)]),
                line(&[(0.50, 0.82), (0.50, 0.28)]),
                line(&[(0.72, 0.82), (0.72, 0.42)]),
            ] },
            // A root with two branches off it. Read as a tree rather than as a
            // flowchart because the branches leave one side only, which is what the
            // default `LayoutKind::Tree` actually draws.
            Self::MindMap => const { &[
                outline(rect_pts!(0.08, 0.40, 0.38, 0.60)),
                outline(rect_pts!(0.62, 0.14, 0.92, 0.34)),
                outline(rect_pts!(0.62, 0.66, 0.92, 0.86)),
                line(&[(0.38, 0.50), (0.50, 0.50), (0.50, 0.24), (0.62, 0.24)]),
                line(&[(0.50, 0.50), (0.50, 0.76), (0.62, 0.76)]),
            ] },
            // Three columns, the middle one carrying a card. Reads as columns rather
            // than as the table icon because the divisions run one way only and the
            // card inside one of them is filled.
            Self::Kanban => const { &[
                outline(rect_pts!(0.10, 0.16, 0.36, 0.84)),
                outline(rect_pts!(0.42, 0.16, 0.68, 0.84)),
                outline(rect_pts!(0.74, 0.16, 0.90, 0.84)),
                solid(&[(0.46, 0.24), (0.64, 0.24), (0.64, 0.40), (0.46, 0.40)]),
            ] },
            Self::Image => const { &[
                outline(rect_pts!(0.16, 0.24, 0.84, 0.76)),
                Prim::Circle { c: (0.35, 0.39), r: 0.06, fill: true },
                line(&[(0.20, 0.72), (0.42, 0.50), (0.55, 0.61), (0.66, 0.51), (0.82, 0.68)]),
            ] },
            Self::BringToFront => const { &[
                line(&[(0.20, 0.14), (0.80, 0.14)]),
                line(&[(0.50, 0.86), (0.50, 0.38)]),
                solid(&[(0.50, 0.26), (0.33, 0.46), (0.67, 0.46)]),
            ] },
            Self::BringForward => const { &[
                line(&[(0.50, 0.86), (0.50, 0.30)]),
                solid(&[(0.50, 0.16), (0.33, 0.38), (0.67, 0.38)]),
            ] },
            Self::SendBackward => const { &[
                line(&[(0.50, 0.14), (0.50, 0.70)]),
                solid(&[(0.50, 0.84), (0.33, 0.62), (0.67, 0.62)]),
            ] },
            Self::SendToBack => const { &[
                line(&[(0.20, 0.86), (0.80, 0.86)]),
                line(&[(0.50, 0.14), (0.50, 0.62)]),
                solid(&[(0.50, 0.74), (0.33, 0.54), (0.67, 0.54)]),
            ] },
            Self::Group => const { &[
                outline(rect_pts!(0.30, 0.30, 0.52, 0.52)),
                outline(rect_pts!(0.48, 0.48, 0.70, 0.70)),
                line(&[(0.14, 0.26), (0.14, 0.14), (0.26, 0.14)]),
                line(&[(0.74, 0.14), (0.86, 0.14), (0.86, 0.26)]),
                line(&[(0.86, 0.74), (0.86, 0.86), (0.74, 0.86)]),
                line(&[(0.26, 0.86), (0.14, 0.86), (0.14, 0.74)]),
            ] },
            Self::Ungroup => const { &[
                outline(rect_pts!(0.14, 0.14, 0.46, 0.46)),
                outline(rect_pts!(0.54, 0.54, 0.86, 0.86)),
            ] },
            Self::Lock => const { &[
                outline(rect_pts!(0.26, 0.46, 0.74, 0.84)),
                line(&[
                    (0.36, 0.46),
                    (0.36, 0.31),
                    (0.41, 0.21),
                    (0.50, 0.18),
                    (0.59, 0.21),
                    (0.64, 0.31),
                    (0.64, 0.46),
                ]),
            ] },
            Self::Unlock => const { &[
                outline(rect_pts!(0.26, 0.46, 0.74, 0.84)),
                line(&[
                    (0.36, 0.46),
                    (0.36, 0.31),
                    (0.41, 0.21),
                    (0.50, 0.18),
                    (0.59, 0.21),
                    (0.63, 0.30),
                ]),
            ] },
            Self::AlignLeft => const { &[
                line(&[(0.16, 0.14), (0.16, 0.86)]),
                solid(rect_pts!(0.24, 0.24, 0.82, 0.44)),
                solid(rect_pts!(0.24, 0.56, 0.60, 0.76)),
            ] },
            Self::AlignCenterHorizontal => const { &[
                line(&[(0.50, 0.12), (0.50, 0.88)]),
                solid(rect_pts!(0.18, 0.24, 0.82, 0.44)),
                solid(rect_pts!(0.32, 0.56, 0.68, 0.76)),
            ] },
            Self::AlignRight => const { &[
                line(&[(0.84, 0.14), (0.84, 0.86)]),
                solid(rect_pts!(0.18, 0.24, 0.76, 0.44)),
                solid(rect_pts!(0.40, 0.56, 0.76, 0.76)),
            ] },
            Self::AlignTop => const { &[
                line(&[(0.14, 0.16), (0.86, 0.16)]),
                solid(rect_pts!(0.24, 0.24, 0.44, 0.82)),
                solid(rect_pts!(0.56, 0.24, 0.76, 0.60)),
            ] },
            Self::AlignMiddleVertical => const { &[
                line(&[(0.12, 0.50), (0.88, 0.50)]),
                solid(rect_pts!(0.24, 0.18, 0.44, 0.82)),
                solid(rect_pts!(0.56, 0.32, 0.76, 0.68)),
            ] },
            Self::AlignBottom => const { &[
                line(&[(0.14, 0.84), (0.86, 0.84)]),
                solid(rect_pts!(0.24, 0.18, 0.44, 0.76)),
                solid(rect_pts!(0.56, 0.40, 0.76, 0.76)),
            ] },
            Self::DistributeHorizontal => const { &[
                solid(rect_pts!(0.12, 0.24, 0.24, 0.76)),
                solid(rect_pts!(0.44, 0.24, 0.56, 0.76)),
                solid(rect_pts!(0.76, 0.24, 0.88, 0.76)),
            ] },
            Self::DistributeVertical => const { &[
                solid(rect_pts!(0.24, 0.12, 0.76, 0.24)),
                solid(rect_pts!(0.24, 0.44, 0.76, 0.56)),
                solid(rect_pts!(0.24, 0.76, 0.76, 0.88)),
            ] },
            Self::ZoomIn => const { &[
                Prim::Circle { c: (0.44, 0.44), r: 0.26, fill: false },
                line(&[(0.63, 0.63), (0.84, 0.84)]),
                line(&[(0.31, 0.44), (0.57, 0.44)]),
                line(&[(0.44, 0.31), (0.44, 0.57)]),
            ] },
            Self::ZoomOut => const { &[
                Prim::Circle { c: (0.44, 0.44), r: 0.26, fill: false },
                line(&[(0.63, 0.63), (0.84, 0.84)]),
                line(&[(0.31, 0.44), (0.57, 0.44)]),
            ] },
            Self::ZoomToFit => const { &[
                line(&[(0.14, 0.36), (0.14, 0.14), (0.36, 0.14)]),
                line(&[(0.64, 0.14), (0.86, 0.14), (0.86, 0.36)]),
                line(&[(0.86, 0.64), (0.86, 0.86), (0.64, 0.86)]),
                line(&[(0.36, 0.86), (0.14, 0.86), (0.14, 0.64)]),
                outline(rect_pts!(0.34, 0.34, 0.66, 0.66)),
            ] },
            Self::Minimap => const { &[
                outline(rect_pts!(0.14, 0.20, 0.86, 0.80)),
                solid(rect_pts!(0.52, 0.46, 0.80, 0.74)),
            ] },
            // The bare hash: a grid has no edge, which is the whole difference
            // between it and `Frame`.
            // Three rules, each a little further apart than the last is thick, so the ramp
            // reads even when every line is rendered at the same stroke width — an icon set
            // that draws all its strokes uniformly cannot show thickness with thickness.
            Self::Thickness => const { &[
                line(&[(0.16, 0.28), (0.84, 0.28)]),
                line(&[(0.16, 0.50), (0.84, 0.50)]),
                line(&[(0.16, 0.54), (0.84, 0.54)]),
                line(&[(0.16, 0.74), (0.84, 0.74)]),
                line(&[(0.16, 0.80), (0.84, 0.80)]),
                line(&[(0.16, 0.86), (0.84, 0.86)]),
            ] },
            // A ring with a checkerboard quarter: the ring says "the whole thing" and the
            // squares are the universal mark for what shows through.
            Self::Opacity => const { &[
                Prim::Circle { c: (0.50, 0.50), r: 0.36, fill: false },
                line(&[(0.50, 0.14), (0.50, 0.86)]),
                line(&[(0.50, 0.32), (0.68, 0.32)]),
                line(&[(0.50, 0.50), (0.86, 0.50)]),
                line(&[(0.50, 0.68), (0.68, 0.68)]),
            ] },
            Self::Grid => const { &[
                line(&[(0.34, 0.10), (0.34, 0.90)]),
                line(&[(0.66, 0.10), (0.66, 0.90)]),
                line(&[(0.10, 0.34), (0.90, 0.34)]),
                line(&[(0.10, 0.66), (0.90, 0.66)]),
            ] },
            // A curved arrow doubling back on itself. The pair is mirrored in x, so
            // they read as one gesture in two directions rather than as two icons.
            Self::Undo => const { &[
                line(&[(0.80, 0.80), (0.74, 0.58), (0.58, 0.44), (0.36, 0.38), (0.22, 0.38)]),
                line(&[(0.38, 0.24), (0.20, 0.38), (0.38, 0.52)]),
            ] },
            Self::Redo => const { &[
                line(&[(0.20, 0.80), (0.26, 0.58), (0.42, 0.44), (0.64, 0.38), (0.78, 0.38)]),
                line(&[(0.62, 0.24), (0.80, 0.38), (0.62, 0.52)]),
            ] },
            Self::Present => const { &[
                outline(rect_pts!(0.14, 0.20, 0.86, 0.64)),
                line(&[(0.50, 0.64), (0.50, 0.80)]),
                line(&[(0.32, 0.82), (0.68, 0.82)]),
                solid(&[(0.43, 0.32), (0.43, 0.52), (0.61, 0.42)]),
            ] },
            Self::TextAlignLeft => const { &[
                line(&[(0.16, 0.24), (0.84, 0.24)]),
                line(&[(0.16, 0.41), (0.60, 0.41)]),
                line(&[(0.16, 0.58), (0.84, 0.58)]),
                line(&[(0.16, 0.75), (0.60, 0.75)]),
            ] },
            Self::TextAlignCenter => const { &[
                line(&[(0.16, 0.24), (0.84, 0.24)]),
                line(&[(0.28, 0.41), (0.72, 0.41)]),
                line(&[(0.16, 0.58), (0.84, 0.58)]),
                line(&[(0.28, 0.75), (0.72, 0.75)]),
            ] },
            Self::TextAlignRight => const { &[
                line(&[(0.16, 0.24), (0.84, 0.24)]),
                line(&[(0.40, 0.41), (0.84, 0.41)]),
                line(&[(0.16, 0.58), (0.84, 0.58)]),
                line(&[(0.40, 0.75), (0.84, 0.75)]),
            ] },
            // A five-pointed star on radii 0.42 and 0.17 about the centre, first
            // point up. The outline is one contour; the fill is the inner pentagon
            // plus five triangles, because `epaint`'s polygon fill assumes convexity
            // and a star is the standard counter-example — filled as one contour it
            // draws a pentagon with five stray wedges.
            Self::Star => const { &[
                outline(&[(0.480, 0.096), (0.492, 0.085), (0.508, 0.085), (0.520, 0.096), (0.616, 0.291), (0.628, 0.308), (0.643, 0.323), (0.662, 0.333), (0.682, 0.339), (0.898, 0.370), (0.912, 0.378), (0.917, 0.394), (0.910, 0.408), (0.754, 0.560), (0.741, 0.576), (0.732, 0.596), (0.728, 0.617), (0.729, 0.638), (0.766, 0.852), (0.762, 0.868), (0.749, 0.877), (0.733, 0.875), (0.541, 0.774), (0.521, 0.767), (0.500, 0.764), (0.479, 0.767), (0.459, 0.774), (0.267, 0.875), (0.251, 0.877), (0.238, 0.868), (0.234, 0.852), (0.271, 0.638), (0.272, 0.617), (0.268, 0.596), (0.259, 0.576), (0.246, 0.560), (0.090, 0.408), (0.083, 0.394), (0.088, 0.378), (0.102, 0.370), (0.317, 0.339), (0.338, 0.333), (0.357, 0.323), (0.372, 0.308), (0.384, 0.291)]),
            ] },
            Self::StarFilled => const { &[
                solid(&[(0.480, 0.096), (0.492, 0.085), (0.508, 0.085), (0.520, 0.096), (0.616, 0.291), (0.628, 0.308), (0.643, 0.323), (0.662, 0.333), (0.682, 0.339), (0.898, 0.370), (0.912, 0.378), (0.917, 0.394), (0.910, 0.408), (0.754, 0.560), (0.741, 0.576), (0.732, 0.596), (0.728, 0.617), (0.729, 0.638), (0.766, 0.852), (0.762, 0.868), (0.749, 0.877), (0.733, 0.875), (0.541, 0.774), (0.521, 0.767), (0.500, 0.764), (0.479, 0.767), (0.459, 0.774), (0.267, 0.875), (0.251, 0.877), (0.238, 0.868), (0.234, 0.852), (0.271, 0.638), (0.272, 0.617), (0.268, 0.596), (0.259, 0.576), (0.246, 0.560), (0.090, 0.408), (0.083, 0.394), (0.088, 0.378), (0.102, 0.370), (0.317, 0.339), (0.338, 0.333), (0.357, 0.323), (0.372, 0.308), (0.384, 0.291)]),
            ] },
            Self::Pin => const { &[
                Prim::Circle { c: (0.50, 0.34), r: 0.18, fill: false },
                line(&[(0.30, 0.52), (0.70, 0.52)]),
                line(&[(0.50, 0.52), (0.50, 0.86)]),
            ] },
            Self::More => const { &[
                Prim::Circle { c: (0.50, 0.20), r: 0.08, fill: true },
                Prim::Circle { c: (0.50, 0.50), r: 0.08, fill: true },
                Prim::Circle { c: (0.50, 0.80), r: 0.08, fill: true },
            ] },
            Self::List => const { &[
                Prim::Circle { c: (0.20, 0.26), r: 0.06, fill: true },
                Prim::Circle { c: (0.20, 0.50), r: 0.06, fill: true },
                Prim::Circle { c: (0.20, 0.74), r: 0.06, fill: true },
                line(&[(0.38, 0.26), (0.84, 0.26)]),
                line(&[(0.38, 0.50), (0.84, 0.50)]),
                line(&[(0.38, 0.74), (0.84, 0.74)]),
            ] },
            // Evenly spaced on the same rows `List` uses, so the two sit at the same
            // optical weight when they appear near each other.
            Self::Hamburger => const { &[
                line(&[(0.18, 0.28), (0.82, 0.28)]),
                line(&[(0.18, 0.50), (0.82, 0.50)]),
                line(&[(0.18, 0.72), (0.82, 0.72)]),
            ] },
            Self::Upload => const { &[
                line(&[(0.50, 0.74), (0.50, 0.40)]),
                solid(&[(0.50, 0.26), (0.35, 0.46), (0.65, 0.46)]),
                line(&[(0.16, 0.62), (0.16, 0.86), (0.84, 0.86), (0.84, 0.62)]),
            ] },
            Self::Search => const { &[
                Prim::Circle { c: (0.44, 0.44), r: 0.26, fill: false },
                line(&[(0.63, 0.63), (0.84, 0.84)]),
            ] },
            Self::Plus => const { &[line(&[(0.20, 0.50), (0.80, 0.50)]), line(&[(0.50, 0.20), (0.50, 0.80)])] },
            Self::Close => const { &[
                line(&[(0.26, 0.26), (0.74, 0.74)]),
                line(&[(0.74, 0.26), (0.26, 0.74)]),
            ] },
            Self::Check => const { &[line(&[(0.22, 0.52), (0.42, 0.72), (0.78, 0.28)])] },
            Self::ChevronUp => const { &[line(&[(0.30, 0.60), (0.50, 0.40), (0.70, 0.60)])] },
            Self::ChevronDown => const { &[line(&[(0.30, 0.40), (0.50, 0.60), (0.70, 0.40)])] },
            Self::ChevronRight => const { &[line(&[(0.40, 0.30), (0.60, 0.50), (0.40, 0.70)])] },
            Self::ChevronLeft => const { &[line(&[(0.60, 0.30), (0.40, 0.50), (0.60, 0.70)])] },
            Self::Import => const { &[
                line(&[(0.50, 0.14), (0.50, 0.54)]),
                solid(&[(0.50, 0.66), (0.35, 0.46), (0.65, 0.46)]),
                line(&[(0.20, 0.60), (0.20, 0.84), (0.80, 0.84), (0.80, 0.60)]),
            ] },
            Self::Duplicate => const { &[
                outline(rect_pts!(0.14, 0.14, 0.60, 0.60)),
                outline(rect_pts!(0.40, 0.40, 0.86, 0.86)),
            ] },
            Self::Trash => const { &[
                line(&[(0.16, 0.26), (0.84, 0.26)]),
                line(&[(0.40, 0.26), (0.40, 0.16), (0.60, 0.16), (0.60, 0.26)]),
                line(&[(0.24, 0.26), (0.29, 0.86), (0.71, 0.86), (0.76, 0.26)]),
            ] },
            Self::Info => const { &[
                Prim::Circle { c: (0.50, 0.50), r: 0.36, fill: false },
                Prim::Circle { c: (0.50, 0.31), r: 0.055, fill: true },
                line(&[(0.50, 0.44), (0.50, 0.72)]),
            ] },
            Self::Folder => const { &[
                outline(&[(0.833, 0.833), (0.855, 0.830), (0.875, 0.822), (0.892, 0.809), (0.906, 0.792), (0.914, 0.772), (0.917, 0.750), (0.917, 0.333), (0.914, 0.312), (0.906, 0.292), (0.892, 0.274), (0.875, 0.261), (0.855, 0.253), (0.833, 0.250), (0.504, 0.250), (0.484, 0.248), (0.464, 0.240), (0.447, 0.228), (0.434, 0.212), (0.400, 0.163), (0.387, 0.147), (0.370, 0.135), (0.351, 0.128), (0.330, 0.125), (0.167, 0.125), (0.145, 0.128), (0.125, 0.136), (0.108, 0.149), (0.094, 0.167), (0.086, 0.187), (0.083, 0.208), (0.083, 0.750), (0.086, 0.772), (0.094, 0.792), (0.108, 0.809), (0.125, 0.822), (0.145, 0.830), (0.167, 0.833)]),
            ] },
            // A terminal prompt inside a box. The chevron and the rule under it are what
            // make this read as "a thing that is running" rather than as another
            // rectangle — the shape tool's square and the image's frame are both already
            // rectangles at this size, so the box alone carries no meaning.
            Self::Agent => const { &[
                outline(rect_pts!(0.12, 0.18, 0.88, 0.82)),
                line(&[(0.28, 0.38), (0.41, 0.50), (0.28, 0.62)]),
                line(&[(0.50, 0.62), (0.72, 0.62)]),
            ] },
            // A page with the fold at the **top** right. `Icon::Sticky` folds at the
            // bottom right, and at 16pt the corner is the only thing distinguishing the
            // two silhouettes — so they fold opposite ways deliberately.
            Self::Note => const { &[
                outline(&[
                    (0.26, 0.10), (0.62, 0.10), (0.76, 0.24),
                    (0.76, 0.90), (0.26, 0.90),
                ]),
                line(&[(0.62, 0.10), (0.62, 0.24), (0.76, 0.24)]),
                line(&[(0.36, 0.44), (0.66, 0.44)]),
                line(&[(0.36, 0.60), (0.66, 0.60)]),
                line(&[(0.36, 0.76), (0.54, 0.76)]),
            ] },
            // A root, a stem, and two branches ending in rows — the shape every file
            // browser draws, and the one thing that cannot be confused with a table.
            Self::FileTree => const { &[
                outline(rect_pts!(0.10, 0.10, 0.42, 0.24)),
                line(&[(0.20, 0.24), (0.20, 0.72)]),
                line(&[(0.20, 0.42), (0.42, 0.42)]),
                outline(rect_pts!(0.44, 0.35, 0.90, 0.49)),
                line(&[(0.20, 0.72), (0.42, 0.72)]),
                outline(rect_pts!(0.44, 0.65, 0.90, 0.79)),
            ] },
            // A window: a frame with a chrome bar and two dots in it. The dots are what
            // say "browser" rather than "picture frame".
            Self::Browser => const { &[
                outline(rect_pts!(0.10, 0.20, 0.90, 0.80)),
                line(&[(0.10, 0.36), (0.90, 0.36)]),
                Prim::Circle { c: (0.20, 0.28), r: 0.032, fill: true },
                Prim::Circle { c: (0.30, 0.28), r: 0.032, fill: true },
            ] },
            // Optically centred, not geometrically: a triangle's centroid sits a third of
            // the way from its base, so a triangle centred on 0.50 reads as sitting left.
            Self::Play => const { &[solid(&[(0.32, 0.20), (0.32, 0.80), (0.82, 0.50)])] },
            Self::Stop => const { &[solid(rect_pts!(0.26, 0.26, 0.74, 0.74))] },
            Self::Sun => const { &[
                Prim::Circle { c: (0.50, 0.50), r: 0.19, fill: true },
                line(&[(0.50, 0.08), (0.50, 0.20)]),
                line(&[(0.50, 0.80), (0.50, 0.92)]),
                line(&[(0.08, 0.50), (0.20, 0.50)]),
                line(&[(0.80, 0.50), (0.92, 0.50)]),
                line(&[(0.20, 0.20), (0.29, 0.29)]),
                line(&[(0.71, 0.71), (0.80, 0.80)]),
                line(&[(0.80, 0.20), (0.71, 0.29)]),
                line(&[(0.29, 0.71), (0.20, 0.80)]),
            ] },
            Self::Moon => const { &[solid(&[
                (0.64, 0.14),
                (0.42, 0.22),
                (0.28, 0.44),
                (0.30, 0.64),
                (0.44, 0.80),
                (0.66, 0.86),
                (0.50, 0.68),
                (0.46, 0.48),
                (0.52, 0.30),
            ])] },
        }
    }

    /// Paints the icon to fill `rect`.
    ///
    /// `width` is the stroke width in points; icons are drawn at a constant stroke
    /// weight rather than one scaled with the box, because a 40px tool button and a
    /// 16px menu icon should read as the same family.
    pub fn paint(self, painter: &Painter, rect: Rect, color: Color32, width: f32) {
        let at = |(x, y): (f32, f32)| Pos2::new(rect.min.x + x * rect.width(), rect.min.y + y * rect.height());
        let stroke = Stroke::new(width, color);
        // Circles are scaled by the shorter side so a non-square box squashes the
        // whole icon uniformly instead of turning circles into ellipses.
        let unit = rect.width().min(rect.height());

        for prim in self.prims() {
            match *prim {
                Prim::Poly { pts, closed, fill } => {
                    let points: Vec<Pos2> = pts.iter().copied().map(at).collect();
                    let shape = if fill {
                        PathShape::convex_polygon(points, color, Stroke::NONE)
                    } else if closed {
                        PathShape::closed_line(points, stroke)
                    } else {
                        PathShape::line(points, stroke)
                    };
                    painter.add(Shape::Path(shape));
                }
                Prim::Circle { c, r, fill } => {
                    let centre = at(c);
                    let radius = r * unit;
                    if fill {
                        painter.circle_filled(centre, radius, color);
                    } else {
                        painter.circle_stroke(centre, radius, stroke);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_ICON: &[Icon] = &[
        Icon::Select,
        Icon::Hand,
        Icon::Sticky,
        Icon::Text,
        Icon::Shape,
        Icon::Pen,
        Icon::Eraser,
        Icon::Connector,
        Icon::Frame,
        Icon::Image,
        Icon::BringToFront,
        Icon::BringForward,
        Icon::SendBackward,
        Icon::SendToBack,
        Icon::Group,
        Icon::Ungroup,
        Icon::Lock,
        Icon::Unlock,
        Icon::AlignLeft,
        Icon::AlignCenterHorizontal,
        Icon::AlignRight,
        Icon::AlignTop,
        Icon::AlignMiddleVertical,
        Icon::AlignBottom,
        Icon::DistributeHorizontal,
        Icon::DistributeVertical,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::ZoomToFit,
        Icon::Minimap,
        Icon::Grid,
        Icon::Present,
        Icon::TextAlignLeft,
        Icon::TextAlignCenter,
        Icon::TextAlignRight,
        Icon::Star,
        Icon::StarFilled,
        Icon::Pin,
        Icon::More,
        Icon::List,
        Icon::Hamburger,
        Icon::Upload,
        Icon::Search,
        Icon::Plus,
        Icon::Close,
        Icon::Check,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::ChevronRight,
        Icon::ChevronLeft,
        Icon::Import,
        Icon::Duplicate,
        Icon::Trash,
        Icon::Info,
        Icon::Sun,
        Icon::Moon,
        Icon::Agent,
        Icon::Note,
        Icon::FileTree,
        Icon::Browser,
        Icon::Play,
        Icon::Stop,
    ];

    /// An icon whose geometry escapes the unit box gets clipped by whatever button
    /// contains it, and the clipping is subtle enough to survive review. Padding of
    /// 0.06 is left so a stroke of a few points does not bleed either.
    #[test]
    fn every_icon_stays_inside_its_unit_box() {
        for icon in EVERY_ICON {
            for prim in icon.prims() {
                match *prim {
                    Prim::Poly { pts, .. } => {
                        assert!(!pts.is_empty(), "{icon:?} has an empty contour");
                        for &(x, y) in pts {
                            assert!(
                                (0.06..=0.94).contains(&x) && (0.06..=0.94).contains(&y),
                                "{icon:?} point ({x}, {y}) leaves the unit box"
                            );
                        }
                    }
                    Prim::Circle { c: (x, y), r, .. } => {
                        assert!(
                            x - r >= 0.06 && x + r <= 0.94 && y - r >= 0.06 && y + r <= 0.94,
                            "{icon:?} circle at ({x}, {y}) r {r} leaves the unit box"
                        );
                    }
                }
            }
        }
    }

    /// A filled polygon needs three points; a stroked polyline needs two. One-point
    /// contours paint nothing at all, which looks like a missing icon.
    #[test]
    fn no_icon_contains_a_degenerate_contour() {
        for icon in EVERY_ICON {
            for prim in icon.prims() {
                if let Prim::Poly { pts, fill, .. } = *prim {
                    let minimum = if fill { 3 } else { 2 };
                    assert!(pts.len() >= minimum, "{icon:?} contour has only {} points", pts.len());
                }
            }
        }
    }

    /// Every icon has to survive tessellation into actual triangles. A contour that
    /// is geometrically valid but degenerate — collinear, zero-area, wound the wrong
    /// way — passes the tests above and then draws as an empty button.
    #[test]
    fn every_icon_tessellates_into_triangles() {
        let ctx = egui::Context::default();
        for icon in EVERY_ICON {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                icon.paint(
                    &ui.painter().clone(),
                    Rect::from_min_size(Pos2::new(4.0, 4.0), egui::vec2(24.0, 24.0)),
                    crate::theme::Palette::LIGHT.text,
                    crate::widgets::ICON_STROKE,
                );
            });
            let triangles: usize = ctx
                .tessellate(output.shapes, 1.0)
                .iter()
                .map(|clipped| match &clipped.primitive {
                    egui::epaint::Primitive::Mesh(mesh) => mesh.indices.len() / 3,
                    egui::epaint::Primitive::Callback(_) => 0,
                })
                .sum();
            assert!(triangles > 0, "{icon:?} tessellated to nothing");
        }
    }
}
