//! What a node looks like — colours, weight, corner radius.
//!
//! This crate deliberately does **not** define a palette. `docs/05-design-language.md`
//! is explicit that colour resolves through theme tokens and that no widget may
//! carry a hex literal, and a mind map is a document object whose colours must
//! survive a light/dark switch. So [`NodeStyle`] is a plain data record that the app
//! fills from the active theme, and the defaults here are the two neutral values a
//! document can hold without asserting anything about the theme: an unfilled node,
//! and the `ink` value that document text uses in either mode.
//!
//! The one opinion this module does hold is [`NodeStyle::connector`]: the colour of
//! the link *into* a node from its parent lives on the **child**, not on the parent
//! and not on the edge. That is how a branch gets one colour all the way out from
//! the root — recolouring a depth-1 node and its subtree recolours the whole branch,
//! which is what a mind map user means by "colour this branch".

use serde::{Deserialize, Serialize};

/// Straight-alpha sRGBA, one byte per channel — the same representation
/// `vellum-doc::Color` uses, so the boundary conversion is a struct literal.
///
/// Straight rather than premultiplied: premultiplication quantises, and a colour
/// that round-trips through the document, a picker and back must come out
/// bit-identical or a user's palette drifts a step darker every time they open it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);

    /// `#1A1D1F` — the `ink` token of `docs/05-design-language.md` §1. Present as a
    /// constant because a node with no text colour at all is not a sensible default
    /// state for a document; the app still overrides it from the theme.
    pub const INK: Self = Self::rgb(0x1A, 0x1D, 0x1F);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 0xFF)
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn is_visible(self) -> bool {
        self.a > 0
    }
}

/// Everything about a node's appearance that layout does not decide.
///
/// Layout owns position and reads [`crate::Node::size`]; it reads nothing here. The
/// separation is what lets the whole of [`crate::layout`] be tested without a single
/// colour appearing in a test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NodeStyle {
    /// Node background. Transparent by default: an unstyled mind map reads as text
    /// on branches, which is what a mind map is, rather than a field of boxes.
    pub fill: Color,
    /// Label colour.
    pub text: Color,
    /// Border colour. Invisible by default — `docs/05-design-language.md` §2 asks
    /// for one hairline doing the work, not a border on everything.
    pub border: Color,
    /// Border width in world px. The design language's hairline.
    pub border_width: f64,
    /// Corner radius in world px. `4.0` is not arbitrary: §2 of the design language
    /// pins radii at 4px, occasionally 6, explicitly ruling out pillowy corners.
    pub corner_radius: f64,
    /// Label size in world px.
    pub font_size: f64,
    /// Whether the label is emboldened. Roots and first-level branches usually are.
    pub bold: bool,
    /// Colour of the connector running from this node's **parent** to this node.
    /// See the module docs for why it lives on the child.
    pub connector: Color,
    /// Width of that connector, in world px.
    pub connector_width: f64,
}

impl Default for NodeStyle {
    fn default() -> Self {
        Self {
            fill: Color::TRANSPARENT,
            text: Color::INK,
            border: Color::TRANSPARENT,
            border_width: 1.0,
            corner_radius: 4.0,
            font_size: 14.0,
            bold: false,
            connector: Color::INK,
            connector_width: 2.0,
        }
    }
}

impl NodeStyle {
    /// Builder-style overrides, so a test or an importer can express "the default
    /// but bold" without restating nine fields.
    pub fn with_fill(mut self, fill: Color) -> Self {
        self.fill = fill;
        self
    }

    pub fn with_text(mut self, text: Color) -> Self {
        self.text = text;
        self
    }

    pub fn with_border(mut self, border: Color, width: f64) -> Self {
        self.border = border;
        self.border_width = width;
        self
    }

    pub fn with_connector(mut self, connector: Color, width: f64) -> Self {
        self.connector = connector;
        self.connector_width = width;
        self
    }

    pub fn bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }

    pub fn with_font_size(mut self, font_size: f64) -> Self {
        self.font_size = font_size;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_style_asserts_nothing_about_the_theme() {
        let s = NodeStyle::default();
        assert!(!s.fill.is_visible(), "an unstyled node is text on a branch, not a box");
        assert!(!s.border.is_visible());
        assert_eq!(s.corner_radius, 4.0, "docs/05-design-language.md §2 pins radii at 4px");
    }

    #[test]
    fn builders_compose_without_restating_the_record() {
        let s = NodeStyle::default().bold(true).with_font_size(18.0);
        assert!(s.bold);
        assert_eq!(s.font_size, 18.0);
        assert_eq!(s.text, NodeStyle::default().text);
    }
}
