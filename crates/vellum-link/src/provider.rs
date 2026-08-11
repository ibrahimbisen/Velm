//! What a URL's site is called, and where its icon lives.
//!
//! Both answers come from the **host**, with no network and no fetch, which is the whole
//! point: a link pasted on the board is named on the very next frame, and an imported board
//! names all 132 of its cards offline. A fetch can improve the name later — a page's own
//! `og:site_name` beats a guess — but it is never needed for the card to read as a card.
//!
//! # Why a table and not just the host
//!
//! `youtube.com` should read *YouTube*, not *Youtube*, and `docs.google.com` should read
//! *Google Docs* rather than *Google*. Casing and product names are not derivable from a
//! domain, so the ones worth getting right are named. Everything else falls back to the
//! registrable label with its first letter capitalised — `techforum.net` becomes *Techforum*,
//! which is what Miro shows for the same link and is honest about being a guess.

/// Sites named explicitly, longest host first so `docs.google.com` wins over `google.com`.
///
/// Matched against the host and against any parent of it, so `www.youtube.com` and
/// `m.youtube.com` both resolve. Ordered by how often a board actually holds one — the
/// reference board is YouTube and forum links, and the reference screenshots are AliExpress
/// and YouTube.
const NAMED: &[(&str, &str)] = &[
    // Video and audio.
    ("youtube.com", "YouTube"),
    ("youtu.be", "YouTube"),
    ("vimeo.com", "Vimeo"),
    ("loom.com", "Loom"),
    ("twitch.tv", "Twitch"),
    ("soundcloud.com", "SoundCloud"),
    ("open.spotify.com", "Spotify"),
    ("spotify.com", "Spotify"),
    ("music.apple.com", "Apple Music"),
    ("podcasts.apple.com", "Apple Podcasts"),
    // Design and documents — the "productivity" embeds Miro leads with.
    ("figma.com", "Figma"),
    ("canva.com", "Canva"),
    ("miro.com", "Miro"),
    ("notion.so", "Notion"),
    ("notion.site", "Notion"),
    ("docs.google.com", "Google Docs"),
    ("sheets.google.com", "Google Sheets"),
    ("slides.google.com", "Google Slides"),
    ("drive.google.com", "Google Drive"),
    ("forms.gle", "Google Forms"),
    ("google.com", "Google"),
    ("airtable.com", "Airtable"),
    ("dropbox.com", "Dropbox"),
    ("sharepoint.com", "SharePoint"),
    ("office.com", "Microsoft 365"),
    ("linear.app", "Linear"),
    ("atlassian.net", "Atlassian"),
    ("slack.com", "Slack"),
    // Code.
    ("github.com", "GitHub"),
    ("gist.github.com", "GitHub Gist"),
    ("gitlab.com", "GitLab"),
    ("bitbucket.org", "Bitbucket"),
    ("stackoverflow.com", "Stack Overflow"),
    ("codepen.io", "CodePen"),
    ("codesandbox.io", "CodeSandbox"),
    ("replit.com", "Replit"),
    ("crates.io", "crates.io"),
    ("docs.rs", "docs.rs"),
    // Social.
    ("twitter.com", "X"),
    ("x.com", "X"),
    ("instagram.com", "Instagram"),
    ("tiktok.com", "TikTok"),
    ("linkedin.com", "LinkedIn"),
    ("reddit.com", "Reddit"),
    ("facebook.com", "Facebook"),
    ("pinterest.com", "Pinterest"),
    ("threads.net", "Threads"),
    ("mastodon.social", "Mastodon"),
    // Shopping — a large share of the links people paste onto a board.
    ("aliexpress.com", "AliExpress"),
    ("aliexpress.us", "AliExpress"),
    ("amazon.com", "Amazon"),
    ("amazon.co.uk", "Amazon"),
    ("amazon.de", "Amazon"),
    ("ebay.com", "eBay"),
    ("etsy.com", "Etsy"),
    ("alibaba.com", "Alibaba"),
    ("mouser.com", "Mouser"),
    ("digikey.com", "DigiKey"),
    ("lcsc.com", "LCSC"),
    ("jlcpcb.com", "JLCPCB"),
    ("thingiverse.com", "Thingiverse"),
    ("printables.com", "Printables"),
    // Reference.
    ("wikipedia.org", "Wikipedia"),
    ("imdb.com", "IMDb"),
    ("medium.com", "Medium"),
    ("substack.com", "Substack"),
    ("arxiv.org", "arXiv"),
    ("news.ycombinator.com", "Hacker News"),
];

/// Second-level suffixes, so `amazon.co.uk` yields `amazon` rather than `co`.
///
/// A short list rather than the Public Suffix List: that list is ~10,000 entries updated
/// weekly, and being wrong here costs a slightly odd fallback name on an unusual domain —
/// not a broken card. The alternative is a dependency that has to be kept current.
const TWO_LABEL_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "co.jp", "co.kr", "co.nz", "co.za", "co.in",
    "com.au", "com.br", "com.cn", "com.mx", "com.tr", "com.tw", "com.sg", "com.hk",
];

/// The host of a URL, lowercased, with `www.` and any port or credentials removed.
///
/// Hand-rolled rather than a URL crate: this is the only URL parsing the workspace needs,
/// and it needs exactly the host. Returns `None` for anything without one — a `mailto:`, a
/// bare word someone pasted, an empty string.
pub fn host_of(url: &str) -> Option<String> {
    let rest = match url.split_once("://") {
        // A scheme is present and says what this is. Anything but the web is refused here
        // rather than downstream, so no caller can be handed a "host" for a `mailto:` — which
        // has an `@` and a domain and parses as one if you are not looking for it.
        Some((scheme, rest)) => {
            let scheme = scheme.trim().to_ascii_lowercase();
            if scheme != "http" && scheme != "https" {
                return None;
            }
            rest
        }
        // No `://`, so either a bare host (`example.com/x`) or a non-hierarchical scheme
        // (`mailto:a@b.com`, `javascript:…`). They are told apart by what follows the colon:
        // a port is digits, a scheme's payload is not.
        None => {
            let before_path = url.split(['/', '?', '#']).next().unwrap_or(url);
            if let Some((_, after)) = before_path.split_once(':')
                && !after.is_empty()
                && !after.chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            url
        }
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    // Credentials, then a port. Both are legal and neither is part of the name.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = authority.split(':').next()?.trim().to_ascii_lowercase();
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    Some(host.strip_prefix("www.").unwrap_or(&host).to_owned())
}

/// The registrable label — `aliexpress` from `www.aliexpress.us`, `amazon` from
/// `amazon.co.uk`.
fn registrable_label(host: &str) -> Option<&str> {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    let two_label_suffix = labels.len() >= 3
        && TWO_LABEL_SUFFIXES.contains(&format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]).as_str());
    let index = if two_label_suffix { labels.len() - 3 } else { labels.len() - 2 };
    labels.get(index).copied()
}

/// The site's name for a URL: `"YouTube"`, `"AliExpress"`, `"Ls1tech"`.
///
/// `None` only when the URL has no host at all, which is the one case where inventing a name
/// would be a lie rather than a guess.
pub fn provider_for(url: &str) -> Option<String> {
    let host = host_of(url)?;
    // Exact host, then each parent domain, so `m.youtube.com` finds `youtube.com` while
    // `docs.google.com` is matched before it can fall through to `google.com`.
    let mut candidate = host.as_str();
    loop {
        if let Some((_, name)) = NAMED.iter().find(|(domain, _)| *domain == candidate) {
            return Some((*name).to_owned());
        }
        match candidate.split_once('.') {
            // Stop before the bare TLD: `com` must never match anything.
            Some((_, parent)) if parent.contains('.') => candidate = parent,
            _ => break,
        }
    }
    Some(capitalise(registrable_label(&host)?))
}

/// First letter up, the rest untouched — `techforum` becomes `Ls1tech` and `eBay` would keep
/// its own casing if it ever reached here, which it does not because it is named above.
fn capitalise(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Where to look for a site's icon, absent a `<link rel="icon">` in its HTML.
///
/// `/favicon.ico` at the host's root, which every browser tries and almost every site
/// answers. Deliberately **not** a third-party favicon service: those work better and would
/// mean telling someone else's server every domain on the user's board.
pub fn favicon_url(url: &str) -> Option<String> {
    let host = host_of(url)?;
    Some(format!("https://{host}/favicon.ico"))
}

/// Whether a card's address is a page whose subject is a **video**.
///
/// Drives the ▶ over a card's poster frame: *"the video previews such as the YouTube previews
/// on Miro I like more because I can just open it and view the video right then and there"*.
/// Velm does not play it — that needs a browser engine, which `docs/01-architecture.md` §1
/// rules out — so the play button opens the page in the browser. Which is why the
/// mark matters more than it would if it were decorative: it is the difference between a card
/// you know is a video and one you have to read to find out.
///
/// # Why a list and not "has a thumbnail"
///
/// Nearly every card has a poster image; almost none of them are videos. Drawing a ▶ on a
/// product photo promises playback that pressing it cannot deliver, which is the same dead
/// button `has_link` already exists to prevent one class of.
///
/// Matched by the same parent-domain walk [`provider_for`] uses, so `m.youtube.com` and
/// `www.youtube.com` both answer — and `youtu.be` is named separately because it shares no
/// parent with the rest.
pub fn plays_video(url: &str) -> bool {
    const VIDEO: &[&str] =
        &["youtube.com", "youtu.be", "vimeo.com", "loom.com", "twitch.tv", "dailymotion.com"];
    let Some(host) = host_of(url) else { return false };
    let mut candidate = host.as_str();
    loop {
        if VIDEO.contains(&candidate) {
            return true;
        }
        match candidate.split_once('.') {
            Some((_, parent)) if parent.contains('.') => candidate = parent,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_stripped_to_what_names_a_site() {
        assert_eq!(host_of("https://www.aliexpress.us/item/1005.html?spm=a2g0o").as_deref(), Some("aliexpress.us"));
        assert_eq!(host_of("http://EXAMPLE.com:8080/x").as_deref(), Some("example.com"));
        assert_eq!(host_of("https://user:pw@git.example.org/repo").as_deref(), Some("git.example.org"));
        assert_eq!(host_of("example.com/no-scheme").as_deref(), Some("example.com"));
        // Nothing that names a site.
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("just a sentence"), None);
        // Schemes that are not the web are refused rather than parsed for a host: a `mailto:`
        // has an `@` and a domain and looks exactly like an authority if nothing checks.
        assert_eq!(host_of("mailto:someone@example.com"), None, "no host in a mailto");
        assert_eq!(host_of("javascript:alert(1)"), None);
        assert_eq!(host_of("file:///etc/passwd"), None);
        assert_eq!(host_of("ftp://example.com/x"), None);
    }

    /// The names worth getting right, and the rule that keeps a subdomain from falling
    /// through to its parent's name.
    #[test]
    fn named_sites_keep_their_own_casing_and_products() {
        assert_eq!(provider_for("https://www.youtube.com/watch?v=abc").as_deref(), Some("YouTube"));
        assert_eq!(provider_for("https://youtu.be/abc").as_deref(), Some("YouTube"));
        assert_eq!(provider_for("https://m.youtube.com/watch?v=abc").as_deref(), Some("YouTube"));
        assert_eq!(provider_for("https://github.com/rust-lang/rust").as_deref(), Some("GitHub"));
        assert_eq!(provider_for("https://www.ebay.com/itm/1").as_deref(), Some("eBay"));
        assert_eq!(provider_for("https://www.imdb.com/title/tt1").as_deref(), Some("IMDb"));
        // A product, not just its parent company.
        assert_eq!(provider_for("https://docs.google.com/document/d/1").as_deref(), Some("Google Docs"));
        assert_eq!(provider_for("https://drive.google.com/file/d/1").as_deref(), Some("Google Drive"));
        assert_eq!(provider_for("https://www.google.com/search?q=x").as_deref(), Some("Google"));
    }

    /// The three shapes a real board's links come in: a named vendor, a site with no
    /// entry in the table, and one reached through a subdomain.
    #[test]
    fn the_boards_own_links_are_named() {
        assert_eq!(provider_for("https://www.aliexpress.us/item/1005000000000.html").as_deref(), Some("AliExpress"));
        assert_eq!(
            provider_for("https://techforum.net/forums/swaps/1-x.html").as_deref(),
            Some("Techforum"),
            "an unnamed site falls back to its own label rather than to nothing"
        );
        assert_eq!(
            provider_for("https://wiki.example.net/en/Docs/integrations").as_deref(),
            Some("Example"),
            "the registrable label, not the subdomain"
        );
    }

    #[test]
    fn a_two_label_suffix_does_not_become_the_name() {
        assert_eq!(provider_for("https://www.amazon.co.uk/dp/B01").as_deref(), Some("Amazon"));
        assert_eq!(provider_for("https://example.co.uk/a").as_deref(), Some("Example"));
        assert_eq!(provider_for("https://shop.example.com.au/a").as_deref(), Some("Example"));
    }

    /// A bare TLD must never be a provider name, which is what a naive parent walk produces
    /// for an unknown two-label host.
    #[test]
    fn a_tld_is_never_a_name() {
        for url in ["https://unknown-site.com/a", "https://thing.io/b", "https://a.b.c.net/d"] {
            let name = provider_for(url).expect("a host yields a name");
            assert!(!["Com", "Io", "Net"].contains(&name.as_str()), "{url} named {name}");
        }
        assert_eq!(provider_for("no-host-at-all"), None);
    }

    #[test]
    fn the_favicon_is_looked_for_on_the_site_itself() {
        assert_eq!(
            favicon_url("https://www.aliexpress.us/item/1.html").as_deref(),
            Some("https://aliexpress.us/favicon.ico")
        );
        assert_eq!(favicon_url("not a url"), None);
    }
}
