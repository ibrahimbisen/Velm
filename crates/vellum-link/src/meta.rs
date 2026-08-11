//! A page's card metadata, read out of its HTML.
//!
//! Pure and offline: HTML in, [`LinkMeta`] out. That split is deliberate and it is where the
//! logic lives — every rule about which tag wins, which fallback applies and how a relative
//! image URL resolves is testable against a string literal, with no network and no fixtures
//! to keep fresh. [`crate::fetch`] is the thin part that goes and gets the string.
//!
//! # Which tags, and in what order
//!
//! OpenGraph is what Miro reads (`docs/02-miro-formats.md` records the reference board's
//! `preview` widgets carrying an `openGraph` block), and it is what nearly every site serves.
//! Twitter's cards are the common second, and the plain `<title>` is the floor — a page with
//! no card markup at all still has a name. So per field:
//!
//! - **title** — `og:title`, then `twitter:title`, then `<title>`.
//! - **description** — `og:description`, then `twitter:description`, then
//!   `<meta name="description">`.
//! - **image** — `og:image`, then `og:image:url`, then `twitter:image`. No `<img>` scraping:
//!   the first image in a page's body is a logo or a tracking pixel far more often than it is
//!   the thing the page is about, and a wrong picture is worse than none.
//! - **site name** — `og:site_name`. Absent, the caller keeps the name
//!   [`crate::provider`] derived from the host, which is why that is not done here.
//! - **icon** — `<link rel="icon">` and its variants, largest declared size first;
//!   `/favicon.ico` is the caller's fallback.
//!
//! A page may also *point* at richer data with an oEmbed discovery link, which is how the
//! provider's own title and poster frame are reached for a video. [`LinkMeta::oembed`] carries
//! it when there is one.

/// Everything a card can learn about a page from its markup.
///
/// Every field is optional in the same way `ItemKind::LinkPreview`'s are, and for the same
/// reason: an absent description is a card with no blurb, which is different from a card with
/// an empty one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    /// Absolute URL of the preview image.
    pub image: Option<String>,
    /// The page's own name for its site, which beats a name guessed from the host.
    pub site_name: Option<String>,
    /// Absolute URL of the site's icon, from a `<link rel="icon">`.
    pub icon: Option<String>,
    /// Absolute URL of an oEmbed endpoint the page advertises.
    pub oembed: Option<String>,
}

impl LinkMeta {
    /// Whether anything at all was found. A page that yields nothing is still a card — it
    /// just shows its URL — but the caller may want to know not to write an empty record.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Reads a page's card metadata. `base` is the URL it was fetched from, for resolving
/// relative image and icon paths.
pub fn parse(html: &str, base: &str) -> LinkMeta {
    let dom = match tl::parse(html, tl::ParserOptions::default()) {
        Ok(dom) => dom,
        // Malformed markup is a page with no metadata, not an error worth propagating: the
        // card still has the URL the user pasted.
        Err(error) => {
            log::debug!("link metadata: {base} would not parse ({error})");
            return LinkMeta::default();
        }
    };
    let parser = dom.parser();

    // One pass over the meta tags, collecting by property/name. Later duplicates lose, which
    // matches how browsers read OpenGraph: the first declaration wins.
    let mut props: Vec<(String, String)> = Vec::new();
    let mut icons: Vec<(Option<u32>, String)> = Vec::new();
    let mut oembed: Option<String> = None;
    let mut doc_title: Option<String> = None;

    for handle in dom.nodes() {
        let Some(tag) = handle.as_tag() else { continue };
        let name = tag.name().as_utf8_str();
        let attr = |key: &str| {
            tag.attributes()
                .get(key)
                .flatten()
                .map(|v| v.as_utf8_str().trim().to_string())
                .filter(|v| !v.is_empty())
        };
        match name.as_ref() {
            "meta" => {
                // `property` is OpenGraph's spelling, `name` is Twitter's and the plain
                // description's. Sites use both, sometimes for the same key.
                let key = attr("property").or_else(|| attr("name"));
                if let (Some(key), Some(content)) = (key, attr("content")) {
                    props.push((key.to_ascii_lowercase(), content));
                }
            }
            "link" => {
                let rel = attr("rel").unwrap_or_default().to_ascii_lowercase();
                let Some(href) = attr("href") else { continue };
                if rel.split_whitespace().any(|r| r == "icon" || r == "shortcut" || r == "apple-touch-icon") {
                    // `sizes="32x32"`; a missing one sorts last, which is right — a declared
                    // size is evidence and no size is not.
                    let size = attr("sizes")
                        .and_then(|s| s.split(['x', 'X']).next().and_then(|n| n.parse::<u32>().ok()));
                    icons.push((size, href));
                } else if rel.contains("alternate")
                    && attr("type").is_some_and(|t| t.contains("oembed"))
                {
                    oembed = oembed.or(Some(href));
                }
            }
            "title" => {
                if doc_title.is_none() {
                    let text = handle.inner_text(parser).trim().to_string();
                    if !text.is_empty() {
                        doc_title = Some(text);
                    }
                }
            }
            _ => {}
        }
    }

    let first = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| props.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    };

    // Largest declared icon first. A card draws a favicon at about 16pt, so any of them will
    // do — but a 16px source scaled up looks worse than a 180px one scaled down.
    icons.sort_by(|a, b| b.0.unwrap_or(0).cmp(&a.0.unwrap_or(0)));

    LinkMeta {
        title: first(&["og:title", "twitter:title"]).or(doc_title).map(|t| decode_entities(&t)),
        description: first(&["og:description", "twitter:description", "description"])
            .map(|d| decode_entities(&d)),
        image: first(&["og:image", "og:image:url", "og:image:secure_url", "twitter:image"])
            .and_then(|url| absolute(&decode_entities(&url), base)),
        site_name: first(&["og:site_name", "application-name"]).map(|s| decode_entities(&s)),
        icon: icons.first().and_then(|(_, href)| absolute(&decode_entities(href), base)),
        oembed: oembed.and_then(|href| absolute(&decode_entities(&href), base)),
    }
}

/// Resolves the HTML entities `tl` leaves in an attribute value.
///
/// `pub` because the **canvas** needs it too, not only the fetch. Decoding here and in the
/// importer fixes what arrives *next*; it does nothing for the text already written into the
/// hundreds of cards on boards that are on disk — the user photographed a YouTube card
/// reading `wouldn&#39;t` on a build that had both fixes in it. `vellum_app::draw::card_text`
/// therefore decodes again at paint time, which repairs every existing board with no
/// migration and no write. Idempotent by construction: a decoded string contains no entity
/// for a second pass to find.
///
/// # Why this is here at all
///
/// `tl` is a *parser*, not a decoder: it hands back attribute values byte for byte, so a page
/// whose `og:description` is `Conversions &amp; Swaps` yields those five characters and the
/// card draws them. Caught in a screenshot of `--demo links` against a live page, on a card
/// that had just been given a properly sized title to be read at — the fix that made the text
/// legible is what made the defect legible with it.
///
/// The URLs are decoded too, and there the stakes are higher than tidiness: an `og:image` or
/// an oembed endpoint carrying `&amp;` in its query string is a *different* address, so the
/// request either 400s or fetches the wrong resource.
///
/// **A deliberate near-duplicate.** `vellum_import::pipeline::richtext::decode_entities` does
/// the same job for the clipboard path, and the two are not shared: `vellum-link` is the
/// network crate and must not depend on the importer, and the alternative — a fifth crate for
/// eleven entities, or pulling in a full HTML-entity table — costs more than the repetition.
/// Both cover what the boards in hand actually contain, and both resolve `&amp;` **last** so
/// that `&amp;lt;` does not collapse to `<`.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    /// The entities the boards in hand actually contain.
    const TABLE: &[(&str, char)] = &[
        ("&quot;", '"'),
        ("&#34;", '"'),
        ("&apos;", '\''),
        ("&#39;", '\''),
        ("&lt;", '<'),
        ("&#60;", '<'),
        ("&gt;", '>'),
        ("&#62;", '>'),
        ("&#43;", '+'),
        ("&#61;", '='),
        ("&nbsp;", ' '),
        ("&#160;", ' '),
        ("&amp;", '&'),
        ("&#38;", '&'),
    ];

    // **One left-to-right pass, never a sequence of `replace`s.**
    //
    // The table used to be applied in order with `str::replace`, guarded by a comment saying
    // `&amp;` must come *last* so that `&amp;lt;` — which is the text `&lt;` — does not
    // collapse to a literal `<`. The comment was right and the code did not honour it:
    // `&#38;` sat *after* `&amp;`, so `&amp;#38;` decoded to `&` in two hops when it should
    // have stopped at `&#38;`. Reordering the pair would have fixed that one case and left
    // the hazard, because *any* entity resolving to `&` reopens it.
    //
    // A single pass cannot have the bug at all: output is appended and never re-examined, so
    // an `&` this function produces is not a candidate for the next match. That is a property
    // of the shape rather than of the order, which is what the comment was reaching for.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match TABLE.iter().find(|(entity, _)| rest.starts_with(entity)) {
            Some((entity, resolved)) => {
                out.push(*resolved);
                rest = &rest[entity.len()..];
            }
            // Not an entity this table knows. Keep the `&` verbatim and carry on past it —
            // `&` is one byte, so this is always a char boundary.
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolves a possibly-relative URL against the page it came from.
///
/// Handles the three forms a real page uses — absolute, protocol-relative (`//cdn/x.png`) and
/// root-relative (`/x.png`) — plus a plain relative path. `None` for anything that cannot be
/// made absolute, so a card never stores a URL nothing can fetch.
pub fn absolute(url: &str, base: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(url.to_owned());
    }
    if let Some(rest) = url.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    // A `data:` image is already self-contained, and there is nothing to resolve it against.
    if url.starts_with("data:") {
        return Some(url.to_owned());
    }
    let host = crate::provider::host_of(base)?;
    if let Some(path) = url.strip_prefix('/') {
        return Some(format!("https://{host}/{path}"));
    }
    // Relative to the page's directory.
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    let path = after_scheme.split(['?', '#']).next().unwrap_or("");
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let dir = dir.split_once('/').map_or("", |(_, rest)| rest);
    if dir.is_empty() {
        Some(format!("https://{host}/{url}"))
    } else {
        Some(format!("https://{host}/{dir}/{url}"))
    }
}

/// The fields an oEmbed JSON response contributes.
///
/// Hand-parsed rather than through `serde_json`, which this crate does not depend on: the
/// response is a flat object and only three of its keys matter. `provider_name` and `title`
/// are what a video's card shows, and `thumbnail_url` is its poster frame — the one piece a
/// page's own `<head>` often does not carry.
pub fn parse_oembed(json: &str) -> LinkMeta {
    LinkMeta {
        title: json_string(json, "title"),
        site_name: json_string(json, "provider_name"),
        image: json_string(json, "thumbnail_url"),
        ..LinkMeta::default()
    }
}

/// Pulls one string value out of a flat JSON object.
///
/// Deliberately minimal, and it stops at the first match. Escapes are resolved for the two
/// sequences oEmbed responses actually contain — `\/` in URLs and `\"` in titles.
fn json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(escaped) => out.push(escaped),
                None => break,
            },
            other => out.push(other),
        }
    }
    Some(out).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://example.com/products/switch.html";

    /// A fetched page's entities must not reach the canvas.
    ///
    /// Found in a screenshot rather than in a test: `--demo links` fetched a real forum page
    /// and the card drew *"Conversions &amp;amp; Swaps - 5.3 wiring harness"*. `tl` returns
    /// attribute values byte for byte, so every `&amp;`, `&#39;` and `&quot;` on the web was
    /// arriving intact. The importer had been taught this for the clipboard path and the
    /// network path had not — the same fix missing from one of two callers, which is a shape
    /// `CLAUDE.md` records more than once.
    #[test]
    fn a_fetched_pages_entities_are_resolved() {
        let html = r#"<head>
            <meta property="og:title" content="Conversions &amp; Swaps &#8212; LS1TECH">
            <meta property="og:description" content="Mark&#39;s &quot;5.3&quot; harness">
            <meta property="og:site_name" content="LS1TECH &amp; Friends">
        </head>"#;
        let meta = parse(html, BASE);
        assert_eq!(meta.title.as_deref(), Some("Conversions & Swaps &#8212; LS1TECH"));
        assert_eq!(meta.description.as_deref(), Some(r#"Mark's "5.3" harness"#));
        assert_eq!(meta.site_name.as_deref(), Some("LS1TECH & Friends"));
    }

    /// A doubly-escaped entity loses exactly one level, never two.
    ///
    /// `&amp;lt;` is the text `&lt;`, and a decoder that turns it into `<` has invented markup
    /// the page did not contain. The sequential-`replace` version got this right for `&amp;`
    /// and wrong for `&#38;`, which sat after it in the table — so `&amp;#38;` decoded twice.
    /// A single left-to-right pass cannot: what it appends is never re-examined.
    #[test]
    fn a_doubly_escaped_entity_loses_exactly_one_level() {
        let title = |content: &str| {
            let html = format!(r#"<head><meta property="og:title" content="{content}"></head>"#);
            parse(&html, BASE).title
        };
        assert_eq!(title("a &amp;lt; b").as_deref(), Some("a &lt; b"));
        assert_eq!(title("a &amp;#38; b").as_deref(), Some("a &#38; b"), "the pair that failed");
        assert_eq!(title("&amp;amp;").as_deref(), Some("&amp;"));
    }

    /// An `&` that begins nothing survives, and so does the text around it.
    #[test]
    fn a_bare_ampersand_is_left_alone() {
        let title = |content: &str| {
            let html = format!(r#"<head><meta property="og:title" content="{content}"></head>"#);
            parse(&html, BASE).title
        };
        assert_eq!(title("Tom &amp; Jerry &amp; Co").as_deref(), Some("Tom & Jerry & Co"));
        assert_eq!(title("R&D and Q&A").as_deref(), Some("R&D and Q&A"));
        assert_eq!(title("trailing &").as_deref(), Some("trailing &"));
        // Multibyte either side of the entity: the pass slices only at `&` and at a known
        // entity's own length, both of which are boundaries.
        assert_eq!(title("汽车 &amp; 排气管").as_deref(), Some("汽车 & 排气管"));
    }

    #[test]
    fn opengraph_is_read_in_full() {
        let html = r#"
            <html><head>
              <title>Ignored when og:title is present</title>
              <meta property="og:title" content="Widget Pro 2.0 Mechanical">
              <meta property="og:description" content="Tip: the Model 33 is for low-profile boards.">
              <meta property="og:image" content="https://cdn.example.com/ks33.jpg">
              <meta property="og:site_name" content="Widget Co">
              <link rel="icon" sizes="32x32" href="/static/favicon-32.png">
            </head><body></body></html>"#;
        let meta = parse(html, BASE);
        assert_eq!(meta.title.as_deref(), Some("Widget Pro 2.0 Mechanical"));
        assert_eq!(meta.description.as_deref(), Some("Tip: the Model 33 is for low-profile boards."));
        assert_eq!(meta.image.as_deref(), Some("https://cdn.example.com/ks33.jpg"));
        assert_eq!(meta.site_name.as_deref(), Some("Widget Co"));
        assert_eq!(meta.icon.as_deref(), Some("https://example.com/static/favicon-32.png"));
        assert!(!meta.is_empty());
    }

    /// The fallback chain, one rung at a time. A page with only a `<title>` is still a card.
    #[test]
    fn each_field_falls_back_in_order() {
        let twitter = parse(
            r#"<head><meta name="twitter:title" content="From Twitter">
               <meta name="twitter:description" content="Blurb">
               <meta name="twitter:image" content="https://cdn/x.png"></head>"#,
            BASE,
        );
        assert_eq!(twitter.title.as_deref(), Some("From Twitter"));
        assert_eq!(twitter.description.as_deref(), Some("Blurb"));
        assert_eq!(twitter.image.as_deref(), Some("https://cdn/x.png"));

        let plain = parse(
            r#"<head><title>  Just a title  </title>
               <meta name="description" content="Plain description"></head>"#,
            BASE,
        );
        assert_eq!(plain.title.as_deref(), Some("Just a title"), "and trimmed");
        assert_eq!(plain.description.as_deref(), Some("Plain description"));
        assert_eq!(plain.image, None, "no image is better than a guessed one");
    }

    /// OpenGraph wins over Twitter, which wins over the document title.
    #[test]
    fn the_richer_source_wins() {
        let html = r#"<head><title>Third</title>
            <meta name="twitter:title" content="Second">
            <meta property="og:title" content="First"></head>"#;
        assert_eq!(parse(html, BASE).title.as_deref(), Some("First"));
    }

    #[test]
    fn relative_image_urls_are_resolved_against_the_page() {
        let root = parse(r#"<head><meta property="og:image" content="/img/a.png"></head>"#, BASE);
        assert_eq!(root.image.as_deref(), Some("https://example.com/img/a.png"));

        let dir = parse(r#"<head><meta property="og:image" content="a.png"></head>"#, BASE);
        assert_eq!(dir.image.as_deref(), Some("https://example.com/products/a.png"));

        let scheme_less =
            parse(r#"<head><meta property="og:image" content="//cdn.example.com/a.png"></head>"#, BASE);
        assert_eq!(scheme_less.image.as_deref(), Some("https://cdn.example.com/a.png"));

        let inline = parse(r#"<head><meta property="og:image" content="data:image/gif;base64,R0lGOD"></head>"#, BASE);
        assert_eq!(inline.image.as_deref(), Some("data:image/gif;base64,R0lGOD"));
    }

    /// The largest declared icon, because a card scales one down and cannot scale one up.
    #[test]
    fn the_biggest_declared_icon_wins() {
        let html = r#"<head>
            <link rel="icon" sizes="16x16" href="/small.png">
            <link rel="apple-touch-icon" sizes="180x180" href="/big.png">
            <link rel="icon" href="/unsized.png"></head>"#;
        assert_eq!(parse(html, BASE).icon.as_deref(), Some("https://example.com/big.png"));
    }

    #[test]
    fn an_oembed_discovery_link_is_picked_up() {
        let html = r#"<head><link rel="alternate" type="application/json+oembed"
            href="https://www.youtube.com/oembed?url=x&amp;format=json"></head>"#;
        let found = parse(html, BASE).oembed.expect("a discovery link");
        assert!(found.starts_with("https://www.youtube.com/oembed?url=x"), "{found}");
    }

    #[test]
    fn a_page_with_no_metadata_yields_nothing_rather_than_blanks() {
        let meta = parse("<html><body><p>hello</p></body></html>", BASE);
        assert!(meta.is_empty());
        assert!(parse("", BASE).is_empty());
        // Malformed markup must not panic. Nothing is asserted about *what* it yields —
        // `tl` is lenient and may recover a tag or none — only that it returns.
        let _ = parse("<<< not html >>>", BASE);
        let _ = parse("<meta property=\"og:title\" content=", BASE);
    }

    /// oEmbed is where a video's poster frame comes from — a `<head>` often has no `og:image`
    /// for one.
    #[test]
    fn oembed_json_gives_the_title_provider_and_poster() {
        let json = r#"{"title":"I Built a Macropad With a Haptic Wheel!",
            "provider_name":"YouTube","provider_url":"https:\/\/www.youtube.com\/",
            "thumbnail_url":"https:\/\/i.ytimg.com\/vi\/abc\/hqdefault.jpg",
            "html":"<iframe ...>"}"#;
        let meta = parse_oembed(json);
        assert_eq!(meta.title.as_deref(), Some("I Built a Macropad With a Haptic Wheel!"));
        assert_eq!(meta.site_name.as_deref(), Some("YouTube"));
        assert_eq!(
            meta.image.as_deref(),
            Some("https://i.ytimg.com/vi/abc/hqdefault.jpg"),
            "the escaped slashes in a JSON URL are resolved"
        );
    }

    #[test]
    fn oembed_parsing_survives_a_response_missing_every_key() {
        assert!(parse_oembed("{}").is_empty());
        assert!(parse_oembed("not json at all").is_empty());
        assert!(parse_oembed(r#"{"title":""}"#).is_empty(), "an empty title is no title");
    }
}
