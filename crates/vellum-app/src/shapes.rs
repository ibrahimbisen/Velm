//! The one place a [`vellum_shapes::Shape`] becomes a document token and back.
//!
//! [`vellum_doc::ItemKind::Shape`] holds an opaque `String` rather than a `Shape`,
//! because `vellum-doc` depends on `loro` and `thiserror` and nothing else and
//! `docs/01-architecture.md` has the dependency arrows pointing strictly downward.
//! This crate depends on both, so this is where the two meet.
//!
//! # Why JSON rather than a name and a list of numbers
//!
//! `Shape::name()` gives 32 stable strings, but the catalogue is *parametric* — a
//! `Star` has a point count and an inner ratio, an `Arrow` has a form, a direction and
//! two proportions — and a name alone throws all of that away. Reconstructing it would
//! need a second table of 32 arms mapping names and positional floats back to
//! variants, kept in step with the first by hand.
//!
//! `Shape` already derives `Serialize`/`Deserialize` and this crate already has
//! `serde_json`, so the round trip is derived rather than written, and adding a shape
//! to the catalogue costs nothing here at all. The cost is that the token is tied to
//! the variants' *spelling* — which a hand-written name table would equally have been.

use vellum_shapes::Shape;

/// The token stored in the document for a shape.
pub fn encode(shape: Shape) -> String {
    serde_json::to_string(&shape).unwrap_or_else(|error| {
        // Cannot happen for a plain data enum of numbers, and losing the shape is
        // worse than losing its parameters, so this degrades rather than fails.
        log::warn!("a shape would not encode ({error}); storing a rectangle");
        RECTANGLE.to_owned()
    })
}

/// The shape a token names.
///
/// **A token this build does not understand becomes a rectangle**, not an error and
/// not a missing item. A board written by a later version — or by a version that spelt
/// a variant differently — still opens, still shows something where the shape was, and
/// still keeps its size, position and label. Refusing the item instead would lose all
/// of that to a name.
pub fn decode(token: &str) -> Shape {
    serde_json::from_str(token).unwrap_or_else(|error| {
        log::warn!("unknown shape `{token}` ({error}); drawing a rectangle");
        Shape::Rectangle
    })
}

/// The token for the shape everything falls back to.
const RECTANGLE: &str = "\"Rectangle\"";

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_shapes::CATALOGUE;

    /// Every shape in the catalogue survives the trip to the document and back —
    /// **parameters included**, which is the whole reason this is not a name table.
    #[test]
    fn every_catalogue_shape_round_trips_with_its_parameters() {
        for shape in CATALOGUE {
            let token = encode(*shape);
            assert_eq!(decode(&token), *shape, "`{}` did not survive as `{token}`", shape.name());
        }
    }

    /// A parametric shape is not merely *a* star after a round trip, it is the same
    /// star. This is the assertion a name-only encoding would fail.
    #[test]
    fn parameters_are_not_quietly_defaulted() {
        let star = Shape::Star { points: 7, inner_ratio: 0.31 };
        assert_eq!(decode(&encode(star)), star);
        // …and two stars that differ only in their parameters encode differently.
        let other = Shape::Star { points: 5, inner_ratio: 0.31 };
        assert_ne!(encode(star), encode(other));
    }

    /// A board from a later build must still open. The item keeps its size, position
    /// and label; only the silhouette falls back.
    #[test]
    fn an_unknown_shape_degrades_to_a_rectangle_rather_than_failing() {
        assert_eq!(decode("\"Dodecahedron\""), Shape::Rectangle);
        assert_eq!(decode(""), Shape::Rectangle);
        assert_eq!(decode("{ not json"), Shape::Rectangle);
    }

    /// The fallback token has to be one `decode` actually accepts, or the degraded
    /// path writes something that degrades again on the next open.
    #[test]
    fn the_fallback_token_decodes() {
        assert_eq!(decode(RECTANGLE), Shape::Rectangle);
        assert_eq!(encode(Shape::Rectangle), RECTANGLE);
    }
}
