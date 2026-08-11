//! The performance HUD: a 5×7 bitmap font drawn through the quad pipeline.
//!
//! # Why not egui
//!
//! `egui` is the plan's choice for real UI chrome and it will arrive with the
//! toolbar and panels. It is the wrong tool for *this*. The HUD is three lines of
//! ASCII, and `egui-wgpu` would couple the renderer's startup to egui's wgpu version
//! — a real constraint while wgpu is on a fast release cadence — pull in a second
//! texture pipeline and its own font atlas, and add ~40 crates to a binary whose
//! whole pitch is being 10 MB instead of 295 MB.
//!
//! A 5×7 font emits one quad per lit pixel through the pipeline that already exists.
//! That is one extra draw call, zero extra dependencies, and it keeps the
//! instrumentation independent of the UI framework — so the HUD still works on a
//! frame where egui itself is what broke.
//!
//! Uppercase only: it halves the glyph table, and a HUD reads fine in caps.

use std::time::Duration;

use vellum_render::{QuadInstance, Rgba};

/// Glyph cell, in font pixels. The 6th column and 8th row are the gap to the next
/// glyph, so callers advance by `ADVANCE` / `LINE_HEIGHT` rather than by 5 / 7.
const GLYPH_WIDTH: usize = 5;
const GLYPH_HEIGHT: usize = 7;
const ADVANCE: f32 = 6.0;
const LINE_HEIGHT: f32 = 9.0;

/// One glyph, row by row, top to bottom. Bit 4 is the leftmost of five columns, so
/// each literal is a picture of the character when the file is read in a monospace
/// font — which is how these were authored and how they should be checked.
type Glyph = [u8; GLYPH_HEIGHT];

#[rustfmt::skip]
const LETTERS: [Glyph; 26] = [
    [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001], // A
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110], // B
    [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110], // C
    [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110], // D
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111], // E
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000], // F
    [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111], // G
    [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001], // H
    [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // I
    [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100], // J
    [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001], // K
    [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111], // L
    [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001], // M
    [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001], // N
    [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110], // O
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000], // P
    [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101], // Q
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001], // R
    [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110], // S
    [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100], // T
    [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110], // U
    [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100], // V
    [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001], // W
    [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001], // X
    [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100], // Y
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111], // Z
];

#[rustfmt::skip]
const DIGITS: [Glyph; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110], // 0
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // 1
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111], // 2
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110], // 3
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010], // 4
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110], // 5
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110], // 6
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000], // 7
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110], // 8
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100], // 9
];

#[rustfmt::skip]
const BLANK: Glyph = [0; GLYPH_HEIGHT];

/// Shown for anything the font does not cover, so a missing glyph is visible on
/// screen rather than an invisible gap that hides a formatting bug.
#[rustfmt::skip]
const TOFU: Glyph = [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111];

#[rustfmt::skip]
fn glyph(c: char) -> Glyph {
    match c.to_ascii_uppercase() {
        ' ' => BLANK,
        'A'..='Z' => LETTERS[(c.to_ascii_uppercase() as u8 - b'A') as usize],
        '0'..='9' => DIGITS[(c as u8 - b'0') as usize],
        '.' => [0, 0, 0, 0, 0, 0b00110, 0b00110],
        ',' => [0, 0, 0, 0, 0, 0b00110, 0b01100],
        ':' => [0, 0b00110, 0b00110, 0, 0b00110, 0b00110, 0],
        '-' => [0, 0, 0, 0b01110, 0, 0, 0],
        '+' => [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
        '/' => [0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        '(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        ')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        '_' => [0, 0, 0, 0, 0, 0, 0b11111],
        _ => TOFU,
    }
}

/// Width in screen pixels of `text` rendered at `pixel` pixels per font pixel.
pub fn text_width(text: &str, pixel: f32) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    // The last glyph contributes its 5 columns but not the trailing gap.
    (text.chars().count() as f32 * ADVANCE - (ADVANCE - GLYPH_WIDTH as f32)) * pixel
}

pub fn line_height(pixel: f32) -> f32 {
    LINE_HEIGHT * pixel
}

/// Appends the quads for one line of text, one quad per lit font pixel.
///
/// `origin` is the top-left corner in screen pixels; `pixel` is how many screen
/// pixels one font pixel occupies, which is where the display's scale factor is
/// applied so the HUD stays the same physical size on a Retina panel.
pub fn push_text(
    out: &mut Vec<QuadInstance>,
    text: &str,
    origin: [f32; 2],
    pixel: f32,
    color: Rgba,
) {
    for (index, c) in text.chars().enumerate() {
        let glyph = glyph(c);
        let left = origin[0] + index as f32 * ADVANCE * pixel;
        for (row, bits) in glyph.iter().enumerate() {
            if *bits == 0 {
                continue;
            }
            for col in 0..GLYPH_WIDTH {
                // Bit 4 is the leftmost column.
                if bits & (1 << (GLYPH_WIDTH - 1 - col)) == 0 {
                    continue;
                }
                out.push(QuadInstance::solid(
                    [left + col as f32 * pixel, origin[1] + row as f32 * pixel],
                    [pixel, pixel],
                    color,
                ));
            }
        }
    }
}

/// Appends a solid rectangle in screen pixels. Used for the HUD's backing panel.
pub fn push_rect(out: &mut Vec<QuadInstance>, origin: [f32; 2], size: [f32; 2], color: Rgba) {
    out.push(QuadInstance::solid(origin, size, color));
}

/// Frame-rate and frame-time statistics.
///
/// Two numbers because they answer different questions. Frame time is smoothed just
/// enough to be readable but still shows a single slow frame; FPS is counted over a
/// fixed window, which is the honest way to report it — the reciprocal of a smoothed
/// frame time flatters the result whenever frame times are uneven.
///
/// Takes an explicit elapsed [`Duration`] rather than reading the clock itself, so
/// the arithmetic is unit-testable without sleeping.
#[derive(Debug, Clone, Default)]
pub struct FrameTimer {
    window_seconds: f64,
    window_frames: u32,
    fps: f64,
    frame_ms: f64,
    total_seconds: f64,
    total_frames: u64,
}

/// How long an FPS sample covers. Short enough to react to a stutter, long enough
/// that the digits are not a blur.
const FPS_WINDOW_SECONDS: f64 = 0.5;

/// Weight of the newest frame in the smoothed frame time.
const FRAME_MS_SMOOTHING: f64 = 0.1;

impl FrameTimer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        let ms = seconds * 1000.0;

        self.frame_ms = if self.total_frames == 0 {
            ms
        } else {
            self.frame_ms + FRAME_MS_SMOOTHING * (ms - self.frame_ms)
        };

        self.total_seconds += seconds;
        self.total_frames += 1;
        self.window_seconds += seconds;
        self.window_frames += 1;

        if self.window_seconds >= FPS_WINDOW_SECONDS {
            self.fps = self.window_frames as f64 / self.window_seconds;
            self.window_seconds = 0.0;
            self.window_frames = 0;
        }
    }

    /// Frames per second over the most recently completed window. Zero until the
    /// first window closes.
    pub fn fps(&self) -> f64 {
        self.fps
    }

    /// Smoothed frame time in milliseconds.
    pub fn frame_ms(&self) -> f64 {
        self.frame_ms
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Frames per second across the whole run. This is the number to quote from a
    /// benchmark; [`FrameTimer::fps`] is what the HUD shows.
    pub fn average_fps(&self) -> f64 {
        if self.total_seconds <= 0.0 {
            return 0.0;
        }
        self.total_frames as f64 / self.total_seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit_pixels(text: &str) -> usize {
        let mut out = Vec::new();
        push_text(&mut out, text, [0.0, 0.0], 1.0, Rgba::WHITE);
        out.len()
    }

    #[test]
    fn every_supported_character_has_a_distinct_glyph() {
        let supported = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.,:-+/%()_";
        for a in supported.chars() {
            for b in supported.chars() {
                if a != b {
                    assert_ne!(glyph(a), glyph(b), "glyphs for '{a}' and '{b}' are identical");
                }
            }
        }
    }

    #[test]
    fn glyphs_fit_inside_five_columns() {
        for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.,:-+/%()_ ".chars() {
            for row in glyph(c) {
                assert!(row < 0b100000, "glyph '{c}' has a bit outside the 5-column cell");
            }
        }
    }

    #[test]
    fn lowercase_folds_to_uppercase() {
        assert_eq!(glyph('a'), glyph('A'));
        assert_eq!(glyph('z'), glyph('Z'));
    }

    #[test]
    fn unknown_characters_render_as_tofu_not_as_nothing() {
        assert_eq!(glyph('€'), TOFU);
        assert_ne!(glyph('€'), BLANK);
    }

    #[test]
    fn space_emits_no_quads() {
        assert_eq!(lit_pixels(" "), 0);
        assert_eq!(lit_pixels("   "), 0);
    }

    #[test]
    fn text_emits_one_quad_per_lit_pixel() {
        // 'I' is 1+1+1+1+1+1+... — count it directly from the table.
        let expected: usize = glyph('I').iter().map(|r| r.count_ones() as usize).sum();
        assert_eq!(lit_pixels("I"), expected);
        assert_eq!(lit_pixels("II"), expected * 2);
    }

    #[test]
    fn glyphs_advance_without_overlapping() {
        let mut out = Vec::new();
        push_text(&mut out, "II", [0.0, 0.0], 2.0, Rgba::WHITE);
        let max_x_of_first = out
            .iter()
            .map(|q| q.origin[0])
            .filter(|x| *x < ADVANCE * 2.0)
            .fold(f32::MIN, f32::max);
        let min_x_of_second = out
            .iter()
            .map(|q| q.origin[0])
            .filter(|x| *x >= ADVANCE * 2.0)
            .fold(f32::MAX, f32::min);
        assert!(max_x_of_first < min_x_of_second, "glyph cells overlap");
    }

    #[test]
    fn text_width_covers_the_drawn_pixels() {
        let text = "FPS 120";
        let pixel = 3.0;
        let mut out = Vec::new();
        push_text(&mut out, text, [0.0, 0.0], pixel, Rgba::WHITE);

        let rightmost = out
            .iter()
            .map(|q| q.origin[0] + q.size[0])
            .fold(f32::MIN, f32::max);
        let width = text_width(text, pixel);
        assert!(rightmost <= width + f32::EPSILON, "{rightmost} > {width}");
        assert!(width - rightmost < ADVANCE * pixel, "width overshoots by a whole cell");
    }

    #[test]
    fn empty_text_has_no_width_and_no_quads() {
        assert_eq!(text_width("", 4.0), 0.0);
        assert_eq!(lit_pixels(""), 0);
    }

    #[test]
    fn scale_multiplies_both_position_and_size() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        push_text(&mut a, "A", [0.0, 0.0], 1.0, Rgba::WHITE);
        push_text(&mut b, "A", [0.0, 0.0], 4.0, Rgba::WHITE);
        assert_eq!(a.len(), b.len());
        for (small, large) in a.iter().zip(&b) {
            assert_eq!(large.origin[0], small.origin[0] * 4.0);
            assert_eq!(large.origin[1], small.origin[1] * 4.0);
            assert_eq!(large.size, [4.0, 4.0]);
        }
    }

    #[test]
    fn fps_is_counted_over_a_window_not_derived_from_one_frame() {
        let mut timer = FrameTimer::new();
        for _ in 0..120 {
            timer.record(Duration::from_secs_f64(1.0 / 120.0));
        }
        assert!((timer.fps() - 120.0).abs() < 1.0, "got {}", timer.fps());
        assert!((timer.frame_ms() - 1000.0 / 120.0).abs() < 0.5, "got {}", timer.frame_ms());
        assert_eq!(timer.total_frames(), 120);
        assert!((timer.average_fps() - 120.0).abs() < 0.001);
    }

    /// Uneven frame times are where a naive `1000 / smoothed_ms` lies. Half the
    /// frames at 4 ms and half at 20 ms average 12 ms, but the honest rate over the
    /// window is 83 fps, not 1000/12 = 83.3 by luck — check the counted value.
    #[test]
    fn uneven_frames_report_the_counted_rate() {
        let mut timer = FrameTimer::new();
        let mut elapsed = 0.0;
        let mut frames = 0;
        while elapsed < 1.0 {
            let dt = if frames % 2 == 0 { 0.004 } else { 0.020 };
            timer.record(Duration::from_secs_f64(dt));
            elapsed += dt;
            frames += 1;
        }
        let expected = frames as f64 / elapsed;
        assert!(
            (timer.average_fps() - expected).abs() < 0.001,
            "average {} vs expected {expected}",
            timer.average_fps()
        );
    }

    #[test]
    fn a_fresh_timer_reports_zero_rather_than_nan() {
        let timer = FrameTimer::new();
        assert_eq!(timer.fps(), 0.0);
        assert_eq!(timer.average_fps(), 0.0);
        assert_eq!(timer.frame_ms(), 0.0);
    }
}
