//! Prints shapes as text, so they can be checked by eye without a GPU.
//!
//! Unit tests pin down the properties a shape must have — closed, filling its box,
//! containing the right points — but none of them can say whether a cloud looks
//! like a cloud. This does, and it has already caught two shapes that passed every
//! test while looking wrong: a cloud whose lobes were too shallow to read as lobes,
//! and a document whose wave came out as a diagonal.
//!
//! ```text
//! cargo run --example ascii            # the whole catalogue
//! cargo run --example ascii -- heart   # one shape by name
//! ```
//!
//! Only the silhouette is drawn: interior detail lines are stroked, never filled,
//! so a cylinder and a database look the same here.

use vellum_shapes::{CATALOGUE, Shape, p};

const COLUMNS: usize = 56;
const ROWS: usize = 26;

fn main() {
    let filter = std::env::args().nth(1);
    let shapes = CATALOGUE
        .iter()
        .filter(|shape| filter.as_deref().is_none_or(|name| shape.name() == name));
    for shape in shapes {
        println!("--- {} {shape:?}", shape.name());
        print(*shape);
    }
}

fn print(shape: Shape) {
    let outline = shape.outline(COLUMNS as f32 / ROWS as f32);
    for row in 0..ROWS {
        let line: String = (0..COLUMNS)
            .map(|column| {
                // Sample the centre of each cell, so a shape's edge never lands
                // exactly on a sample and renders as a coin toss.
                let at = p(
                    (column as f32 + 0.5) / COLUMNS as f32,
                    (row as f32 + 0.5) / ROWS as f32,
                );
                if outline.contains(at) { '#' } else { '.' }
            })
            .collect();
        println!("{line}");
    }
}
