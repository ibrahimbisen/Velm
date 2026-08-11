//! Link cards: what a URL is called, and what a page says about itself.
//!
//! ```no_run
//! # fn main() -> Result<(), vellum_link::LinkError> {
//! // Offline, instant, no network: enough to draw a card.
//! assert_eq!(vellum_link::provider_for("https://youtu.be/abc").as_deref(), Some("YouTube"));
//!
//! // And the richer version, which needs one HTTP request.
//! let card = vellum_link::fetch("https://example.com")?;
//! # let _ = card;
//! # Ok(())
//! # }
//! ```
//!
//! # The split, and why it matters
//!
//! Three quarters of this crate is **pure**: [`provider`] names a site from its host, and
//! [`meta`] reads OpenGraph, Twitter cards and oEmbed out of HTML that is handed to it. Both
//! are tested against string literals, offline, with no fixtures to keep fresh and no flake.
//! [`fetch`] is the thin remainder that actually goes to the network, and it is the only place
//! in the whole workspace that does.
//!
//! That is not tidiness for its own sake. A card has to be drawable **before** any request
//! finishes — a pasted link shows its site and its URL on the very next frame — so the offline
//! answer is the primary one and the fetch is an improvement on it. It also means the app
//! works with networking switched off, which is a setting this app offers.
//!
//! # What is deliberately not here
//!
//! **No live embedding.** A playable video or a live Figma frame needs a browser engine per
//! embed, which `docs/01-architecture.md` §1 rules out and an 8GB machine cannot afford: each
//! `WKWebView` is a separate content process at 60–150MB idle and 250–400MB playing. This
//! crate produces a *card* — a poster frame, a title, a provider — and clicking it opens the
//! page in the browser the user already has. `ItemKind::Embed` still stores the provider's
//! iframe markup, so nothing is lost if that decision is ever revisited.

pub mod fetch;
pub mod meta;
pub mod provider;

pub use fetch::{FetchOptions, LinkError, fetch, fetch_with, get_bytes};
pub use meta::{LinkMeta, decode_entities, parse, parse_oembed};
pub use provider::{favicon_url, host_of, plays_video, provider_for};

/// Everything known about a link, ready for a card.
///
/// The difference from [`LinkMeta`] is that this is *resolved*: the provider is never empty
/// because it falls back to the host's name, and the image and icon URLs are absolute. A
/// caller can draw this without consulting anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCard {
    /// The URL the card points at, as given.
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The site's name. Always present when the URL has a host — the page's own
    /// `og:site_name` when it offers one, otherwise the name derived from the host.
    pub provider: Option<String>,
    /// Absolute URL of the preview image, if the page offers one.
    pub image: Option<String>,
    /// Absolute URL of the site's icon. Falls back to `/favicon.ico`.
    pub icon: Option<String>,
}

impl LinkCard {
    /// The card that can be drawn with no network at all: the URL, and the site's name.
    ///
    /// What a freshly pasted link looks like before anything is fetched, and what it stays as
    /// when fetching is switched off or fails.
    pub fn offline(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            provider: provider_for(&url),
            icon: favicon_url(&url),
            url,
            title: None,
            description: None,
            image: None,
        }
    }

    /// Folds fetched metadata over the offline card.
    ///
    /// Field by field rather than wholesale: a response missing a title must not erase the
    /// host-derived provider, and this is what keeps "fetched but sparse" strictly better than
    /// "not fetched".
    pub fn with_meta(mut self, meta: &LinkMeta) -> Self {
        if meta.title.is_some() {
            self.title = meta.title.clone();
        }
        if meta.description.is_some() {
            self.description = meta.description.clone();
        }
        if meta.image.is_some() {
            self.image = meta.image.clone();
        }
        if meta.site_name.is_some() {
            self.provider = meta.site_name.clone();
        }
        if meta.icon.is_some() {
            self.icon = meta.icon.clone();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offline card is the floor, and everything a fetch returns is an improvement on it
    /// — never a regression. This is the invariant that lets a card draw immediately.
    #[test]
    fn fetched_metadata_only_ever_improves_the_offline_card() {
        let offline = LinkCard::offline("https://www.aliexpress.us/item/1.html");
        assert_eq!(offline.provider.as_deref(), Some("AliExpress"));
        assert_eq!(offline.icon.as_deref(), Some("https://aliexpress.us/favicon.ico"));
        assert_eq!(offline.title, None);

        // A response that carries only a title leaves everything else standing.
        let sparse = LinkMeta { title: Some("Low profile switches".into()), ..LinkMeta::default() };
        let card = offline.clone().with_meta(&sparse);
        assert_eq!(card.title.as_deref(), Some("Low profile switches"));
        assert_eq!(card.provider.as_deref(), Some("AliExpress"), "not erased by a sparse fetch");
        assert_eq!(card.icon, offline.icon);

        // And a full one replaces the guesses with the page's own answers.
        let full = LinkMeta {
            title: Some("Model 33".into()),
            description: Some("Tip: low profile only".into()),
            image: Some("https://cdn/x.png".into()),
            site_name: Some("Widget Co Official".into()),
            icon: Some("https://cdn/icon.png".into()),
            oembed: None,
        };
        let card = offline.with_meta(&full);
        assert_eq!(card.provider.as_deref(), Some("Widget Co Official"), "the page beats the host");
        assert_eq!(card.image.as_deref(), Some("https://cdn/x.png"));
        assert_eq!(card.icon.as_deref(), Some("https://cdn/icon.png"));
    }

    /// A string with no host still makes a card — it just has nothing to say about the site.
    #[test]
    fn a_url_with_no_host_still_yields_a_card() {
        let card = LinkCard::offline("not a url");
        assert_eq!(card.provider, None);
        assert_eq!(card.icon, None);
        assert_eq!(card.url, "not a url");
    }
}
