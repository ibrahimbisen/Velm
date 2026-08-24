//! The size a new picture arrives at, and what counts as a blob's name.
//!
//! # Why this is here and not in the two places that use it
//!
//! Both halves of Velm can put a picture on a board — `vellum_app::actions` from the Mac
//! pasteboard, `vellum_web::pictures` from a paste event or a file picker — and they have to
//! agree. A 5,120px screenshot that arrives 1,200 units wide on the Mac and 5,120 wide in a
//! tab is the *same board* disagreeing with itself about a picture somebody just added.
//!
//! The rule was written twice before it was written here, which is the shape of defect
//! `vellum_web::tools`'s header names: *"the pure decisions are free functions over plain
//! values, so they can be moved to `vellum-project`, where the tests run."* `vellum-web` is
//! `#![cfg(target_arch = "wasm32")]` and compiles no test at all, so a rule that lives only
//! there is a rule nothing checks.

/// The longest edge a newly placed picture is given, in world units.
///
/// A screenshot off a 5K display is 5,120px on its long side, and placed at its own size it
/// arrives larger than the visible board and mostly off-screen. Scaled down it still lands
/// bigger than a sticky, which is the size relationship that reads.
pub const MAX_PLACED_PICTURE: f64 = 1_200.0;

/// The size a picture is given when nothing could measure it.
///
/// Reached only when a decode failed. The item still has to have a size: a placement with a
/// zero side is one the R-tree cannot index and the hit-test can never find.
pub const FALLBACK_SIZE: (f64, f64) = (480.0, 360.0);

/// A picture's own pixels, capped to [`MAX_PLACED_PICTURE`] on its long edge.
///
/// The aspect ratio is kept, so a picture arrives the shape it is. Neither side is ever
/// below `1.0` — see [`FALLBACK_SIZE`] for why a zero side is not an option.
#[must_use]
pub fn fitted(width: f64, height: f64) -> (f64, f64) {
    let (width, height) = measured(width, height);
    let scale = (MAX_PLACED_PICTURE / width.max(height)).min(1.0);
    ((width * scale).max(1.0), (height * scale).max(1.0))
}

/// A reported size, or [`FALLBACK_SIZE`] when it is not one.
///
/// ⚠ **Non-finite is a real input, not a defensive flourish.** `wasm_bindgen` passes
/// JavaScript's `undefined` across as `NaN`, so a call with its arguments left off arrives as
/// a pair of `NaN`s — the trap `vellum_web::clip::velm_note_paste_aim` documents at the other
/// door. On the Mac side the same case is a decoder that could not read the header.
#[must_use]
pub fn measured(width: f64, height: f64) -> (f64, f64) {
    if width.is_finite() && height.is_finite() && width >= 1.0 && height >= 1.0 {
        (width, height)
    } else {
        FALLBACK_SIZE
    }
}

/// Whether a string is exactly what `velmd` names a blob by: 64 hexadecimal characters.
///
/// ⚠ **This guards a path segment.** An `asset_id` is board content, `POST /sync` lets
/// anybody put `"../../../tmp/x"` there, and both clients turn one into a URL —
/// `vellum_app::push::hashes_of` states the same rule for the upload side. Checking the
/// alphabet as well as the length is the whole defence.
///
/// Case is not required. `blake3::Hash::to_hex` writes lowercase, and an upper-cased copy
/// still names the same bytes.
#[must_use]
pub fn is_blob_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_under_the_cap_is_left_alone() {
        assert_eq!(fitted(800.0, 600.0), (800.0, 600.0));
        assert_eq!(fitted(1_200.0, 1_200.0), (1_200.0, 1_200.0));
    }

    #[test]
    fn a_picture_keeps_its_shape_and_loses_its_scale() {
        let (w, h) = fitted(5_120.0, 2_880.0);
        assert!((w - MAX_PLACED_PICTURE).abs() < 1e-9, "long edge {w}");
        assert!((w / h - 5_120.0 / 2_880.0).abs() < 1e-9, "ratio {}", w / h);
        // A tall one caps on *its* long edge, which is the height.
        let (w, h) = fitted(400.0, 4_000.0);
        assert!((h - MAX_PLACED_PICTURE).abs() < 1e-9, "long edge {h}");
        assert!(w >= 1.0);
    }

    /// A zero side is a placement the R-tree cannot index and the hit-test can never find.
    #[test]
    fn no_side_is_ever_zero() {
        let (w, h) = fitted(10_000.0, 1.0);
        assert!(w >= 1.0 && h >= 1.0, "{w}x{h}");
        let (w, h) = fitted(1.0, 10_000.0);
        assert!(w >= 1.0 && h >= 1.0, "{w}x{h}");
    }

    #[test]
    fn an_unmeasurable_picture_still_gets_a_size() {
        assert_eq!(measured(f64::NAN, f64::NAN), FALLBACK_SIZE);
        assert_eq!(measured(0.0, 0.0), FALLBACK_SIZE);
        assert_eq!(measured(f64::INFINITY, 100.0), FALLBACK_SIZE);
        assert_eq!(measured(-4.0, 100.0), FALLBACK_SIZE);
        assert_eq!(measured(640.0, 480.0), (640.0, 480.0));
        // And `fitted` goes through it, so a NaN cannot reach the multiplication.
        assert_eq!(fitted(f64::NAN, f64::NAN), FALLBACK_SIZE);
    }

    #[test]
    fn only_64_hexadecimal_characters_are_a_blob_name() {
        assert!(is_blob_hash(&"a".repeat(64)));
        assert!(is_blob_hash(&"F".repeat(64)));
        assert!(is_blob_hash(&"0123456789abcdef".repeat(4)));
        assert!(!is_blob_hash(""));
        assert!(!is_blob_hash(&"a".repeat(63)));
        assert!(!is_blob_hash(&"a".repeat(65)));
        assert!(!is_blob_hash("../../../etc/passwd"));
        // 64 characters and not all hex — the case a length check alone would pass, and the
        // one that would put a traversal in a URL.
        assert!(!is_blob_hash(&format!("{}z", "a".repeat(63))));
        assert!(!is_blob_hash(&format!("{}/", "a".repeat(63))));
    }
}
