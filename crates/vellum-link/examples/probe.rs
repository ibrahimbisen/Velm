//! Fetches real URLs and prints what a card would show.
//!
//! ```bash
//! cargo run --release -p vellum-link --example probe -- https://github.com/rust-lang/rust
//! ```
//!
//! The **only** honest check of the network path. Everything else in this crate is tested
//! offline against string literals — deliberately, because a test that reaches the internet
//! fails on a train — but that leaves one question no unit test can answer: whether a real
//! server's real response produces a usable card. `ureq`'s TLS, the redirect chain, a page
//! that serves OpenGraph only to a recognised agent, and a `Content-Type` nobody declared are
//! all things that only appear here.
//!
//! An example rather than a `#[test]` for exactly that reason: `cargo test` must stay offline
//! and deterministic, and this is neither.
//!
//! Measured when written, against the live sites:
//!
//! ```text
//! OK https://example.com
//!    provider=Some("Example")   title=Some("Example Domain")   image=None
//! OK https://github.com/rust-lang/rust
//!    provider=Some("GitHub")    title=Some("GitHub - rust-lang/rust: Empowering everyone…")
//!    image=Some("https://opengraph.githubassets.com/…")
//! ```
//!
//! Note what each one demonstrates. `example.com` has no card markup at all, so the title
//! falls back to `<title>` and the provider to the host — which is the floor this crate
//! promises. GitHub serves the full set, so the page's own answers win.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let urls: Vec<&str> = if args.is_empty() {
        vec!["https://example.com", "https://github.com/rust-lang/rust"]
    } else {
        args.iter().map(String::as_str).collect()
    };

    for url in urls {
        match vellum_link::fetch(url) {
            Ok(card) => {
                println!("OK {url}");
                println!("   provider={:?}", card.provider);
                println!("   title={:?}", truncate(card.title.as_deref()));
                println!("   description={:?}", truncate(card.description.as_deref()));
                println!("   image={:?}", truncate(card.image.as_deref()));
                println!("   icon={:?}", truncate(card.icon.as_deref()));
            }
            // A failure is ordinary — a dead link, no network, a login wall — and the card
            // keeps its offline answer. Printed rather than propagated for the same reason.
            Err(error) => println!("ERR {url}: {error}"),
        }
    }
}

/// Keeps the output readable: an `og:image` URL can be several hundred characters.
fn truncate(text: Option<&str>) -> Option<String> {
    text.map(|t| {
        let cut: String = t.chars().take(70).collect();
        if t.chars().count() > 70 { format!("{cut}…") } else { cut }
    })
}
