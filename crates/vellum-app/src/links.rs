//! Fetching link cards, off the frame loop.
//!
//! A card is drawable the moment it exists — `vellum_link::provider_for` names its site from
//! the URL's host with no network at all — so everything here is an *improvement* on a card
//! that already works: the page's own title, its blurb, and the preview image that makes
//! `CardMode::Large` worth choosing.
//!
//! # Why a thread pool and a channel
//!
//! An HTTP request takes between 50ms and the timeout. Doing one on the frame loop would drop
//! frames for as long as it took, and doing 91 would freeze the app for a minute. So requests
//! go to a small pool of worker threads and answers come back through a channel that
//! [`crate::actions::ActiveState`] drains once per frame — the same shape autosave already
//! uses, and the reason neither needs an async runtime.
//!
//! The pool is **two threads**, not one per card. Two keeps a slow server from blocking the
//! rest while staying polite to a host that is about to be asked for several pages: a board of
//! AliExpress links is 40 requests to one origin, and 40 at once is what a rate limiter is for.
//!
//! # What is fetched, and when
//!
//! Nothing automatically. `Library::link_previews` is **off** until the user turns it on,
//! because a board that fetches on open tells a third party every link the user has saved.
//! With it on, a fetch is requested when a link is pasted and when the user asks for one —
//! never on a timer, and never twice for the same URL in a session ([`Fetcher::seen`]).
//!
//! # Two requests, one card
//!
//! The metadata and the image are separate GETs, and the image is the big one — a 1200×630
//! `og:image` is a few hundred KB. It is fetched only when the card is in a mode that draws
//! it, so a board of collapsed rows costs one small request each. The bytes go into the
//! **blob store** under their BLAKE3 hash, which is the same place imported images live, so a
//! fetched preview is cached forever, deduplicated across cards that share an image, and
//! managed by the residency budget that already governs every texture.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use vellum_doc::ItemId as DocId;
use vellum_link::{FetchOptions, LinkCard};

/// How many fetches run at once. See the module note on politeness.
const WORKERS: usize = 2;

/// What a worker was asked to do.
struct Request {
    item: DocId,
    url: String,
    /// Whether to fetch the preview image as well as the metadata.
    want_image: bool,
}

/// What a worker found. Every field is optional because a fetch is allowed to half-succeed:
/// a page that answers but serves no `og:image` still improves the card's title.
pub struct Fetched {
    pub item: DocId,
    pub card: LinkCard,
    /// The preview image's bytes, if one was fetched. Encoded as the server served them —
    /// PNG, JPEG or WebP — which is what the blob store wants.
    pub image: Option<Vec<u8>>,
    /// The site icon's bytes. Fetched for **every** card, unlike the preview image, because a
    /// favicon leads all three display modes and is a couple of kilobytes.
    pub icon: Option<Vec<u8>>,
}

/// The pool, its channels, and the set of URLs already asked for.
pub struct Fetcher {
    outbound: Sender<Request>,
    inbound: Receiver<Fetched>,
    /// URLs requested this session, so a card cannot be fetched twice — by a second paste of
    /// the same link, or by the user pressing refresh twice. Not persisted: the *result* is
    /// persisted, in the document and the blob store, and a new session asking again for a
    /// card that failed last time is the correct behaviour rather than a bug.
    seen: HashSet<String>,
    /// How many requests are out. Reported in the HUD and used to say "fetching 3 cards".
    outstanding: usize,
}

impl Fetcher {
    /// Starts the pool. The threads live for the process and idle on an empty channel.
    pub fn new() -> Self {
        let (outbound, requests) = channel::<Request>();
        let (answers, inbound) = channel::<Fetched>();
        // One receiver shared by every worker, behind a mutex: `mpsc::Receiver` is not `Sync`,
        // and a mutex around `recv` is the standard way to fan one queue out to a pool.
        let requests = std::sync::Arc::new(std::sync::Mutex::new(requests));

        for index in 0..WORKERS {
            let requests = std::sync::Arc::clone(&requests);
            let answers = answers.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("velm-link-{index}"))
                .spawn(move || {
                    loop {
                        // The lock is held only to take a request, never across the fetch —
                        // otherwise the pool would be one thread with extra steps.
                        let request = {
                            let queue = match requests.lock() {
                                Ok(queue) => queue,
                                // A panicking sibling poisoned it. Nothing here is left half
                                // written — a request is a struct of owned strings — so
                                // carrying on is safe and stopping would silently disable
                                // previews for the session.
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            queue.recv()
                        };
                        // The sender was dropped: the app is shutting down.
                        let Ok(request) = request else { return };
                        let found = run(&request);
                        // The receiver was dropped, likewise.
                        if answers.send(found).is_err() {
                            return;
                        }
                    }
                });
            if let Err(error) = spawned {
                log::warn!("link previews: worker {index} would not start ({error})");
            }
        }

        Self { outbound, inbound, seen: HashSet::new(), outstanding: 0 }
    }

    /// Asks for a card's metadata, unless this URL has already been asked for.
    ///
    /// Returns whether a request was actually sent, so a caller can report "fetching 3 of 7"
    /// rather than counting cards it skipped.
    pub fn request(&mut self, item: DocId, url: &str, want_image: bool) -> bool {
        // Refused here rather than in the worker, so a board full of `mailto:` links costs
        // nothing at all. `host_of` answers `None` for every non-http scheme.
        if vellum_link::host_of(url).is_none() {
            return false;
        }
        // Keyed on the URL and the item together: two cards pointing at the same page both
        // want filling in, and the image is deduplicated by content hash further down anyway.
        //
        // `want_image` is part of the key, and leaving it out was a real bug. A card polled
        // while it is a one-line `CardMode::Link` asks for metadata only; switching it to
        // Card or Large then asks again *with* the image, and a key that ignored the flag
        // refused that second request as a duplicate — so the picture never arrived for the
        // rest of the session and only a restart cleared it. The cost of including it is one
        // extra metadata fetch for a card whose mode changed, which is the fetch that has to
        // happen anyway to get the image.
        let key = format!("{item}\u{1}{}\u{1}{url}", u8::from(want_image));
        if !self.seen.insert(key) {
            return false;
        }
        let sent = self
            .outbound
            .send(Request { item, url: url.to_owned(), want_image })
            .is_ok();
        if sent {
            self.outstanding += 1;
        }
        sent
    }

    /// Everything that has come back since the last call. Never blocks.
    pub fn drain(&mut self) -> Vec<Fetched> {
        let mut out = Vec::new();
        loop {
            match self.inbound.try_recv() {
                Ok(found) => {
                    self.outstanding = self.outstanding.saturating_sub(1);
                    out.push(found);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }

    /// How many fetches are still out.
    pub const fn outstanding(&self) -> usize {
        self.outstanding
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

/// One request, on a worker thread.
///
/// Never fails: a card that could not be fetched keeps the offline answer, which is a site
/// name and a URL. Returning an error would give the caller nothing to do but log it.
fn run(request: &Request) -> Fetched {
    let options = FetchOptions::default();
    let card = vellum_link::fetch_with(&request.url, options).unwrap_or_else(|error| {
        log::debug!("link previews: {} ({error})", request.url);
        LinkCard::offline(&request.url)
    });

    let bytes_of = |url: &str, what: &str| match vellum_link::get_bytes(url, options) {
        Ok(bytes) if !bytes.is_empty() => Some(bytes),
        Ok(_) => None,
        Err(error) => {
            log::debug!("link previews: {what} {url} ({error})");
            None
        }
    };

    // The image, only if the card's mode will draw it and the page offered one. It is the big
    // request — a 1200×630 `og:image` is a few hundred KB — so a board of collapsed rows never
    // makes it.
    let image = request
        .want_image
        .then_some(card.image.as_deref())
        .flatten()
        .and_then(|url| bytes_of(url, "image"));

    // The icon, always. Every mode draws one, and it is the fastest thing on a card to read —
    // a favicon is recognised before any of the words are. A few kilobytes, and content-hashed,
    // so forty AliExpress cards share exactly one blob.
    let icon = card.icon.as_deref().and_then(|url| bytes_of(url, "icon"));

    Fetched { item: request.item, card, image, icon }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// The de-duplication, which is what stops a board of 91 cards firing 182 requests when
    /// the user presses refresh twice.
    #[test]
    fn a_url_is_requested_once_per_item() {
        let mut fetcher = Fetcher::new();
        let a = DocId::from_str("1@1").expect("a valid id");
        let b = DocId::from_str("2@1").expect("a valid id");

        assert!(fetcher.request(a, "https://example.com/x", false), "the first ask goes");
        assert!(!fetcher.request(a, "https://example.com/x", false), "the second does not");
        assert!(
            fetcher.request(b, "https://example.com/x", false),
            "a different card wants the same page filled in"
        );
    }

    /// Anything that is not a web page is refused before a thread is woken.
    #[test]
    fn only_web_pages_are_requested() {
        let mut fetcher = Fetcher::new();
        let id = DocId::from_str("1@1").expect("a valid id");
        for refused in ["mailto:a@b.com", "file:///etc/passwd", "javascript:alert(1)", "", "x"] {
            assert!(!fetcher.request(id, refused, false), "{refused} was requested");
        }
        assert_eq!(fetcher.outstanding(), 0);
    }

    /// Draining an idle fetcher is a no-op rather than a block, because it runs every frame.
    #[test]
    fn draining_nothing_returns_nothing_and_does_not_block() {
        let mut fetcher = Fetcher::new();
        assert!(fetcher.drain().is_empty());
        assert_eq!(fetcher.outstanding(), 0);
    }
}
