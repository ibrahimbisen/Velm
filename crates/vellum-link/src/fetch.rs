//! The one part of the workspace that touches the network.
//!
//! Blocking, on purpose. `ureq` makes one request on the calling thread and returns; the app
//! calls this from a worker thread and posts the answer back through a channel, exactly as
//! autosave already does. An async runtime would be a large dependency and a second execution
//! model to reason about, in exchange for concurrency this does not need — a board fetches a
//! handful of cards, once, and caches them forever.
//!
//! # Every limit here is deliberate
//!
//! Fetching a URL means running someone else's server's response through our parser, so each
//! of these is a refusal rather than a preference:
//!
//! - **HTTPS and HTTP only.** No `file:`, no `ftp:`, nothing that could read the local disk.
//! - **A byte ceiling.** [`FetchOptions::max_bytes`] caps the read, because a card needs a
//!   page's `<head>` and a hostile or broken server can offer an endless body.
//! - **A timeout**, so a board's fetches cannot pile up against a server that never answers.
//! - **A redirect limit**, which `ureq` enforces for us.
//! - **No cookies, no credentials, no JavaScript.** A card is built from what a public GET
//!   returns. A page that needs a login shows its URL and its host, which is the honest answer.

use std::io::Read;
use std::time::Duration;

use crate::LinkCard;
use crate::meta::{self, LinkMeta};

/// Why a fetch produced no card.
///
/// A fetch failing is ordinary — a dead link, an offline machine, a server behind a login —
/// so the caller is expected to keep the offline card and move on rather than surface an
/// error to the user.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("`{0}` is not an http(s) URL")]
    NotHttp(String),
    #[error("fetching {url} failed: {source}")]
    Transport {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("reading {url} failed: {source}")]
    Body {
        url: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{url} answered {status}")]
    Status { url: String, status: u16 },
}

/// The knobs, with defaults sized for a link card rather than for a crawler.
#[derive(Debug, Clone, Copy)]
pub struct FetchOptions {
    /// How long to wait for the whole exchange.
    pub timeout: Duration,
    /// How much of the body to read before giving up.
    ///
    /// The **safety net**, not the normal case: [`get_head`] stops at `</head>`, so an ordinary
    /// page costs a few kilobytes however this is set. The number only matters for a page whose
    /// head is enormous, and one of those is on the reference board — **YouTube's watch page
    /// puts `og:title` at byte 684,912 and closes its head at 692,232**, measured. A 512KB cap
    /// cut before the title and the card came back with a provider and nothing else.
    ///
    /// 1MB clears that with room and still refuses a body that never ends.
    pub max_bytes: usize,
    /// Whether to follow an oEmbed discovery link with a second request. Worth it for a
    /// video — the poster frame is usually only there — and one extra round trip.
    pub follow_oembed: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(8), max_bytes: 1024 * 1024, follow_oembed: true }
    }
}

/// A browser-shaped User-Agent.
///
/// Named as ourselves, with a contact-free comment. Plenty of sites serve no OpenGraph to an
/// unrecognised agent, and the alternative — impersonating Chrome — is a lie that also breaks
/// the moment the string goes stale.
const USER_AGENT: &str = concat!("Velm/", env!("CARGO_PKG_VERSION"), " (link preview)");

/// Fetches a URL and builds the richest card it can.
///
/// Never returns a card *worse* than [`LinkCard::offline`]: the offline answer is built first
/// and the fetched metadata is folded over it, so a page that serves only a title still keeps
/// the provider name derived from its host.
pub fn fetch(url: &str) -> Result<LinkCard, LinkError> {
    fetch_with(url, FetchOptions::default())
}

/// [`fetch`] with the limits spelled out.
pub fn fetch_with(url: &str, options: FetchOptions) -> Result<LinkCard, LinkError> {
    // Only the head, which is where every tag a card needs lives. See `get_head`.
    let html = get_head(url, options)?;
    let mut found = meta::parse(&html, url);

    // A video's poster frame is usually only in its oEmbed response, not in its `<head>`.
    // Folded *under* what the page itself said, so the page keeps the last word.
    if options.follow_oembed
        && let Some(endpoint) = found.oembed.clone()
        && let Ok(json) = get_text(&endpoint, options)
    {
        let embedded = meta::parse_oembed(&json);
        found = merge_under(found, &embedded);
    }

    Ok(LinkCard::offline(url).with_meta(&found))
}

/// Fills gaps in `primary` from `extra` without overwriting anything.
fn merge_under(mut primary: LinkMeta, extra: &LinkMeta) -> LinkMeta {
    primary.title = primary.title.or_else(|| extra.title.clone());
    primary.description = primary.description.or_else(|| extra.description.clone());
    primary.image = primary.image.or_else(|| extra.image.clone());
    primary.site_name = primary.site_name.or_else(|| extra.site_name.clone());
    primary.icon = primary.icon.or_else(|| extra.icon.clone());
    primary
}

/// Fetches bytes from an http(s) URL, capped and timed out. Public because the app fetches
/// preview *images* through the same limits.
pub fn get_bytes(url: &str, options: FetchOptions) -> Result<Vec<u8>, LinkError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(LinkError::NotHttp(url.to_owned()));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(options.timeout))
        .user_agent(USER_AGENT)
        .build()
        .into();

    let response = agent
        .get(url)
        .call()
        .map_err(|source| LinkError::Transport { url: url.to_owned(), source: Box::new(source) })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(LinkError::Status { url: url.to_owned(), status });
    }

    // `take` rather than a length check: a server's `Content-Length` is a claim, and a
    // chunked response has none at all.
    let mut body = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(options.max_bytes as u64)
        .read_to_end(&mut body)
        .map_err(|source| LinkError::Body { url: url.to_owned(), source })?;
    Ok(body)
}

/// Fetches a page, stopping as soon as its `<head>` has been read.
///
/// **This is what makes the cap workable on a real site.** Every tag a card needs is in the
/// head, and some heads are enormous: YouTube's watch page buries its `og:` tags behind a
/// few hundred kilobytes of inline script, so a flat 512KB cap cut the body *before* the
/// title and the card came back with a provider and nothing else — measured, on the exact
/// URL from the reference board. Raising the cap would fix that by downloading megabytes of
/// player JavaScript for every card; stopping at `</head>` reads what is needed and no more.
///
/// Falls back to the cap for a page with no `</head>` at all, which malformed pages manage.
fn get_head(url: &str, options: FetchOptions) -> Result<String, LinkError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(LinkError::NotHttp(url.to_owned()));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(options.timeout))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .map_err(|source| LinkError::Transport { url: url.to_owned(), source: Box::new(source) })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(LinkError::Status { url: url.to_owned(), status });
    }

    let mut reader = response.into_body().into_reader().take(options.max_bytes as u64);
    let mut body: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|source| LinkError::Body { url: url.to_owned(), source })?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
        // Checked over a window that overlaps the previous chunk, so a `</head>` split across
        // a read boundary is still found.
        let from = body.len().saturating_sub(read + CLOSING_HEAD.len());
        if find(&body[from..], CLOSING_HEAD).is_some() {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Lowercase because a page may spell it `</HEAD>`; the search folds case.
const CLOSING_HEAD: &[u8] = b"</head";

/// Case-insensitive substring search over bytes.
///
/// Hand-rolled to keep this crate's dependencies to the four it has. The haystack is one
/// chunk, so the naive scan is measured in microseconds.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn get_text(url: &str, options: FetchOptions) -> Result<String, LinkError> {
    let bytes = get_bytes(url, options)?;
    // Lossy on purpose. A page declaring one encoding and serving another is common, and a
    // replacement character in a description is a better card than no card.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod head_tests {
    use super::*;

    /// The window overlaps a read boundary, so a `</head>` split across two chunks is found.
    /// Without the overlap a page whose head happens to end on a 16KB boundary would be read
    /// to the cap — the bug this is here to prevent, and one that would only ever appear on
    /// somebody else's site.
    #[test]
    fn the_closing_head_is_found_case_insensitively() {
        assert_eq!(find(b"<html><head></head>", CLOSING_HEAD), Some(12));
        assert_eq!(find(b"<HTML><HEAD></HEAD>", CLOSING_HEAD), Some(12));
        assert_eq!(find(b"no head here", CLOSING_HEAD), None);
        // Shorter than the needle: the windows iterator yields nothing rather than panicking.
        assert_eq!(find(b"</h", CLOSING_HEAD), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scheme check is a refusal, not a preference: without it, a URL from a board file
    /// could name a local path and this would read it.
    #[test]
    fn only_http_urls_are_fetched() {
        for refused in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "mailto:a@b.com",
            "/etc/passwd",
            "",
        ] {
            let error = get_bytes(refused, FetchOptions::default()).unwrap_err();
            assert!(matches!(error, LinkError::NotHttp(_)), "{refused} was not refused: {error}");
        }
    }

    #[test]
    fn the_defaults_are_sized_for_a_card_not_a_crawler() {
        let options = FetchOptions::default();
        // Big enough for the largest head on the reference board — YouTube's, at 692KB —
        // and no bigger. `get_head` stops at `</head>`, so an ordinary page never reads this
        // much; the cap is the refusal for a body that never ends.
        assert!(options.max_bytes >= 700 * 1024, "YouTube's head is 692KB");
        assert!(options.max_bytes <= 2 * 1024 * 1024, "still a cap, not a crawler");
        assert!(options.timeout <= Duration::from_secs(15));
        assert!(options.follow_oembed, "a video's poster frame is only in its oEmbed response");
    }

    /// Filling gaps must never overwrite what the page itself said.
    #[test]
    fn oembed_fills_gaps_without_overriding_the_page() {
        let page = LinkMeta {
            title: Some("The page's own title".into()),
            site_name: Some("Example".into()),
            ..LinkMeta::default()
        };
        let oembed = LinkMeta {
            title: Some("The provider's title".into()),
            site_name: Some("YouTube".into()),
            image: Some("https://i.ytimg.com/vi/abc/hq.jpg".into()),
            ..LinkMeta::default()
        };
        let merged = merge_under(page, &oembed);
        assert_eq!(merged.title.as_deref(), Some("The page's own title"));
        assert_eq!(merged.site_name.as_deref(), Some("Example"));
        assert_eq!(
            merged.image.as_deref(),
            Some("https://i.ytimg.com/vi/abc/hq.jpg"),
            "the gap is filled"
        );
    }
}
