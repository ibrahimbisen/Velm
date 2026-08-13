//! Browser nodes on the board: the document token, and the two things a node can be.
//!
//! A browser node is the one feature in this layer that works directly against Velm's
//! stated identity — an embedded engine is 60–150MB idle and `docs/01-architecture.md` §1
//! rules a webview out of the canvas. So it is gated twice, and the gating is the design:
//!
//! - The **app-wide setting** says browser nodes are permitted at all. Off by default.
//! - The node's own [`BrowserModel::live`] says the user asked *this* page to load.
//!
//! Both, or the node draws as a card naming the page with a button that opens it in the
//! real browser. That is a legible answer rather than a dead rectangle, and — the part that
//! matters for RULE ZERO — the board round-trips identically either way. A board made on a
//! machine with the engine enabled opens correctly on one without it.
//!
//! Collapsing the two switches into one would mean that enabling the setting loads every
//! browser node on every board at once, which is exactly the memory event the setting
//! exists to prevent.

use vellum_agent::BrowserModel;

pub use crate::agent::Rect;

/// The token stored in the document for a browser node.
pub fn encode(model: &BrowserModel) -> String {
    serde_json::to_string(model).unwrap_or_else(|error| {
        log::warn!("a browser node would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The page a token names, or an empty node when it cannot be read. Never fails.
pub fn decode(token: &str) -> BrowserModel {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable browser node ({error}); drawing an empty one");
        }
        BrowserModel::default()
    })
}

/// What the browser tool places. A 16:10 viewport plus the chrome bar above it.
pub const DEFAULT_SIZE: (f64, f64) = (720.0, 480.0);

pub const MIN_SIZE: (f64, f64) = (160.0, 80.0);

const PAD: f64 = 8.0;
const BAR_HEIGHT: f64 = 28.0;
const CONTROL: f64 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserLayout {
    pub bounds: Rect,
    /// The chrome bar: reload, the address, and open-in-browser.
    pub bar: Rect,
    pub reload: Rect,
    /// The address. Editable on the canvas.
    pub address: Rect,
    /// Opens the page in the user's real browser. **Present in both states** — it is the
    /// whole answer when no engine is running, and still the right escape hatch when one is.
    pub open_external: Rect,
    /// Where the page goes, or where the placeholder card is drawn.
    pub viewport: Rect,
    pub too_small: bool,
}

impl BrowserLayout {
    pub fn hit(&self, x: f64, y: f64) -> Option<BrowserPart> {
        if self.too_small {
            return None;
        }
        for (rect, part) in [
            (self.reload, BrowserPart::Reload),
            (self.open_external, BrowserPart::OpenExternal),
            (self.address, BrowserPart::Address),
            (self.viewport, BrowserPart::Viewport),
        ] {
            if !rect.is_empty() && rect.contains(x, y) {
                return Some(part);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserPart {
    Reload,
    Address,
    OpenExternal,
    /// The page itself, or the placeholder standing in for it.
    Viewport,
}

pub fn layout(width: f64, height: f64) -> BrowserLayout {
    let bounds = Rect::new(0.0, 0.0, width.max(0.0), height.max(0.0));
    if width < MIN_SIZE.0 || height < MIN_SIZE.1 {
        return BrowserLayout {
            bounds,
            bar: Rect::default(),
            reload: Rect::default(),
            address: Rect::default(),
            open_external: Rect::default(),
            viewport: bounds,
            too_small: true,
        };
    }

    let inner = bounds.inset(PAD);
    let bar = Rect::new(inner.x, inner.y, inner.width, BAR_HEIGHT);
    let control_y = bar.y + (bar.height - CONTROL) / 2.0;
    let reload = Rect::new(bar.x, control_y, CONTROL, CONTROL);
    let open_external = Rect::new(bar.x + bar.width - CONTROL, control_y, CONTROL, CONTROL);

    // The address takes what is between the two controls, laid out from both ends so a
    // long URL is clipped rather than drawn over a button.
    let address_x = reload.x + CONTROL + PAD / 2.0;
    let address =
        Rect::new(address_x, bar.y, (open_external.x - PAD / 2.0 - address_x).max(0.0), bar.height);

    let top = bar.y + bar.height + PAD / 2.0;
    let viewport =
        Rect::new(inner.x, top, inner.width, (inner.y + inner.height - top).max(0.0));

    BrowserLayout { bounds, bar, reload, address, open_external, viewport, too_small: false }
}

/// Whether this node should actually instantiate an engine.
///
/// Both switches, named in one place so the painter, the runtime and the inspector cannot
/// come to disagree about whether a page is live — which is the failure that would show up
/// as an engine running behind a node that says it is not.
pub const fn should_run_engine(model: &BrowserModel, browser_nodes_enabled: bool) -> bool {
    browser_nodes_enabled && model.live
}

/// What the placeholder says when no engine is running, so the node is never inert.
pub fn placeholder_reason(model: &BrowserModel, browser_nodes_enabled: bool) -> Option<&'static str> {
    match (browser_nodes_enabled, model.live) {
        (true, true) => None,
        (false, _) => Some("Browser nodes are off — turn them on in Preferences"),
        (true, false) => Some("Press Load to open this page on the canvas"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_token_degrades_to_an_empty_node() {
        assert_eq!(decode("nope"), BrowserModel::default());
        let page = BrowserModel { url: "https://example.com".into(), ..BrowserModel::default() };
        assert_eq!(decode(&encode(&page)), page);
    }

    /// Both switches, or no engine. Collapsing them would make enabling the setting load
    /// every browser node on every board at once.
    #[test]
    fn an_engine_needs_both_the_setting_and_the_nodes_own_yes() {
        let dormant = BrowserModel { url: "https://example.com".into(), ..BrowserModel::default() };
        let asked = BrowserModel { live: true, ..dormant.clone() };

        assert!(!should_run_engine(&dormant, false));
        assert!(!should_run_engine(&dormant, true), "the setting alone started an engine");
        assert!(!should_run_engine(&asked, false), "the node alone started an engine");
        assert!(should_run_engine(&asked, true));
    }

    /// Nothing in this app is inert: every state that does not draw a page says why, and
    /// the two reasons are different because the remedies are different.
    #[test]
    fn a_node_with_no_engine_always_says_why() {
        let dormant = BrowserModel::default();
        let asked = BrowserModel { live: true, ..BrowserModel::default() };

        assert!(placeholder_reason(&dormant, false).unwrap().contains("Preferences"));
        assert!(placeholder_reason(&dormant, true).unwrap().contains("Load"));
        assert_ne!(placeholder_reason(&dormant, false), placeholder_reason(&dormant, true));
        assert_eq!(placeholder_reason(&asked, true), None, "a live page still showed a placeholder");
    }

    /// The escape hatch must exist in *both* states — it is the whole answer with no engine,
    /// and still the right button with one.
    #[test]
    fn open_in_the_real_browser_is_always_reachable() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert!(!l.open_external.is_empty());
        let (cx, cy) = (
            l.open_external.x + l.open_external.width / 2.0,
            l.open_external.y + l.open_external.height / 2.0,
        );
        assert_eq!(l.hit(cx, cy), Some(BrowserPart::OpenExternal));
    }

    #[test]
    fn the_address_yields_to_the_controls_on_both_sides() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert!(l.address.x >= l.reload.x + l.reload.width, "the address sat on the reload button");
        assert!(
            l.address.x + l.address.width <= l.open_external.x,
            "the address ran into the open button"
        );
        assert!(l.viewport.y >= l.bar.y + l.bar.height, "the page overlapped its own chrome");
    }

    #[test]
    fn no_part_escapes_the_node() {
        for (w, h) in [DEFAULT_SIZE, MIN_SIZE, (2000.0, 90.0), (170.0, 1200.0)] {
            let l = layout(w, h);
            for rect in [l.bar, l.reload, l.address, l.open_external, l.viewport] {
                if rect.is_empty() {
                    continue;
                }
                assert!(
                    rect.x >= -0.001 && rect.x + rect.width <= w + 0.001,
                    "{rect:?} escaped a {w}x{h} node horizontally"
                );
                assert!(
                    rect.y >= -0.001 && rect.y + rect.height <= h + 0.001,
                    "{rect:?} escaped a {w}x{h} node vertically"
                );
            }
        }
    }
}
