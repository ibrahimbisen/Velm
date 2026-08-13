//! Web research for an agent: fetch a page and read it, or search for one.
//!
//! `docs/07-agent-canvas.md` §12 is the contract. This module is the whole of it, and
//! `crate::mcp` is the only thing that calls it — an agent reaches it as the MCP tools
//! `research_fetch` and `research_search`.
//!
//! # The stated policy, and the line it draws
//!
//! A bare programmatic `GET` is refused by a great many sites, and that refusal is a real
//! limit on what an agent can research. The answer here is to be a **well-behaved browsing
//! client**, not a disguised one. The distinction is the module's whole design, so it is
//! written down rather than left to be inferred:
//!
//! **What this does**, because a normal browser does it and a server is entitled to expect it:
//!
//! - **Identifies itself honestly.** [`DEFAULT_USER_AGENT`] says *Velm*, names the version and
//!   points at the project. It does not claim to be Chrome. A site that decides to refuse Velm
//!   gets to make that decision with the facts.
//! - **Keeps a cookie jar** ([`CookieJar`]), so a session survives across requests the way a
//!   browser's does — the consent banner answered once, the redirect chain that sets a token
//!   completed rather than looped.
//! - **Sends the `Accept` and `Accept-Language` headers a browser sends**, because a server
//!   that content-negotiates has nothing to negotiate with otherwise.
//! - **Reuses connections** — one [`ureq::Agent`] per [`Research`], which pools them.
//! - **Follows redirects itself**, re-checking `robots.txt` and re-pacing on every hop
//!   (see [`Research::fetch_at`]). The point of following them by hand is exactly that
//!   re-check: a redirect to another host is a request to another host.
//! - **Paces itself per host** ([`RateLimiter`]), with a minimum interval and with a
//!   `Crawl-delay` honoured where one is published.
//! - **Fetches, parses and honours `robots.txt`** ([`Robots`]).
//! - **Caps the response** ([`ResearchConfig::max_bytes`]) so a hostile or broken server
//!   cannot hand an agent an endless body.
//!
//! **What this deliberately does not do**, and the reason is one sentence: the goal is robust
//! access, not evasion. A site's access controls are that site's decision, and a research tool
//! that defeats them makes the whole feature indefensible the first time it is noticed.
//!
//! - No IP rotation, no proxy pool.
//! - No browser fingerprint spoofing, and no pretending to be a specific real browser.
//! - No CAPTCHA solving, and no attempt to route around one.
//! - No ignoring `robots.txt`, and no second try with the check turned off.
//! - **A refusal is reported as a refusal.** [`ResearchError::Refused`] carries the status code
//!   and [`ResearchError::RobotsDisallowed`] names the rule. Neither is retried in disguise.
//!   An agent told *"amazon.com answered 403"* can go and find another source; an agent handed
//!   an empty page cannot.
//!
//! # ⚠ The host policy: this tool reaches the public internet and nothing else
//!
//! The URL `research_fetch` is given is **chosen by a model**, from a page it read, from a
//! search result, or from a board file that arrived from somewhere. `crate::mcp` passes it
//! straight through. So the interesting question is not what a site allows us to read, it is
//! what a *machine* allows us to read — and a bare `GET` from inside Velm's process is a `GET`
//! from inside the user's network, with whatever that network trusts about its own address
//! space. That is server-side request forgery, and the scheme check that used to be the only
//! gate here does not touch it: `http://` is exactly the scheme `http://169.254.169.254/…`
//! has.
//!
//! `Research::check_host` therefore refuses, **by literal address and by every address the
//! name resolves to**:
//!
//! - **Loopback** — `127.0.0.0/8`, `::1`, and `localhost` by name. This is where a user's own
//!   Ollama, their SearXNG, Velm's own IPC server and every unauthenticated development tool
//!   on the machine listen.
//! - **Link-local** — `169.254.0.0/16` and `fe80::/10`. `169.254.169.254` is cloud instance
//!   metadata: credentials, in plain text, to anything that can make an HTTP request.
//! - **Private and carrier-grade NAT** — `10/8`, `172.16/12`, `192.168/16`, `100.64/10`, and
//!   IPv6 unique-local `fc00::/7`. The router, the NAS, the printer, the other machines.
//! - **Names that can only mean the local network** — `.local`, `.internal`, `.home.arpa`.
//!
//! Three things about it are deliberate and worth not undoing:
//!
//! - **It is checked before `robots.txt`, not after.** `Research::fetch_robots` makes a real
//!   request to the host, so a policy consulted after it has already let a request out to the
//!   address it was supposed to refuse. And [`Robots::permissive`] is the degraded answer for
//!   an unreachable `robots.txt`, so a local address was not merely unchecked — it was
//!   *explicitly allowed*.
//! - **It is checked on every redirect hop**, for the same reason the robots check is: a
//!   remote page answering `302 Location: http://127.0.0.1:11434/…` is a request to the local
//!   network wearing a public host's clothes.
//! - **A refusal is a [`ResearchError::BlockedHost`], not a network error.** An agent told
//!   *"could not reach it"* retries; an agent told *"Velm does not fetch private addresses"*
//!   goes and finds a public source. This is the same rule the rest of this module already
//!   follows for a 403.
//!
//! **The opt-in, and how it is actually reached.** A user pointing this at their own local
//! model or their own SearXNG is a legitimate thing to want, so
//! [`ResearchConfig::allow_local_hosts`] exists — **default off**, and set by the user through
//! [`ALLOW_LOCAL_HOSTS_ENV`], never by a model. ⚠ For a while it was set by *nobody*: the field
//! was written in one place, `Default::default()`, as `false`, while the refusal's own message
//! told the user to turn it on. An escape hatch that needs a recompile is not one, and a
//! refusal that names it is then giving an instruction that cannot be followed. The distinction that matters throughout is *who chose the URL*: a **search
//! endpoint is user configuration** and is exempt already ([`Research::search_at`]), because
//! SearXNG on `localhost` is the ordinary way to run it and the user typed that address
//! themselves. A URL a model produced is not.
//!
//! **The residual, stated rather than over-claimed: DNS rebinding is not closed.** The
//! addresses are checked here and `ureq` resolves the name again when it connects, so a
//! resolver that answers differently between the two calls defeats this. Closing it needs a
//! connector that dials the address that was checked, which is a `ureq` `Connector`
//! implementation and a larger change than the hole justifies today.
//!
//! # What this module does not read
//!
//! **The clock, except in one place.** Every pure part here — the limiter, the robots cache's
//! expiry — takes `now` as a parameter, so *"is this host still inside its interval"* is
//! arithmetic in a unit test rather than a `sleep`. [`Research::fetch`] and
//! [`Research::search`] are the two convenience wrappers that call `Instant::now` once and
//! delegate to the `_at` form; they are already blocking on a socket, so they are already
//! outside the reach of an offline test. Nothing else in the module looks at a clock.
//!
//! **The network, in `cargo test`.** Every test below runs against a string literal. The live
//! check belongs in an example, the way `vellum-link`'s `probe` already does it.

use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The numbers, in one place
// ---------------------------------------------------------------------------------------

/// How Velm introduces itself.
///
/// Honest and boring: a name, a version, a URL. This is the header a site's operator reads in
/// a log when they wonder who we are, so it has to be the truth and it has to be enough to act
/// on. Impersonating a browser would work slightly better today, be a lie, and break the week
/// the copied string goes stale.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "Velm/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ibrahimbisen/Velm; agent research tool)"
);

/// The name `robots.txt` groups are matched against, lowercase.
///
/// A site that wants to allow or refuse Velm specifically writes `User-agent: velm`. Matched
/// case-insensitively against the whole token, so `Velm` and `VELM` work too.
pub const ROBOTS_TOKEN: &str = "velm";

/// What a browser asks for. Note what is **absent**: `Accept-Encoding`.
///
/// `ureq` is built here with neither the `gzip` nor the `brotli` feature, so it adds no
/// `Accept-Encoding` header and cannot decompress one. Advertising an encoding we cannot
/// decode would turn every compressed page into binary noise, so the header stays off — this
/// is the one respect in which the request is deliberately less browser-like than it could be,
/// and it is a correctness requirement rather than an oversight.
const ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/// A language preference, so a server that content-negotiates has something to negotiate with.
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// The response cap, in bytes.
///
/// **2 MiB, and the number is measured rather than round.** `vellum-link` fetches only a page's
/// `<head>` and needs 1 MiB for it, because YouTube's watch page puts `og:title` at byte
/// **684,912** and closes its head at **692,232**. This module wants the *body* — the readable
/// text is entirely behind that head — so a cap sized like `vellum-link`'s would truncate a
/// page like that the moment the article began. 2 MiB clears the largest heads seen on the
/// reference board with the whole article behind them, and still refuses a body that never ends.
///
/// A page cut at the cap is reported as [`Page::truncated`] rather than silently shortened: an
/// agent that read half an article and did not know it is worse than one that has to ask again.
pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// How much readable text a fetch hands back.
///
/// Separate from [`DEFAULT_MAX_BYTES`] because they answer different questions: the byte cap
/// protects *this process* from an endless body, and this one protects the **model's context**
/// from a 500,000-character page. 40,000 characters is a long article and a small fraction of
/// any current context window.
pub const DEFAULT_MAX_TEXT_CHARS: usize = 40_000;

/// The shortest gap between two requests to the same host.
///
/// One and a half seconds. Slower than a browser loading a page's sub-resources and far slower
/// than a crawler, which is the right side to be on: this is an agent reading a handful of
/// pages, not an index being built. A published `Crawl-delay` raises it and never lowers it.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(1500);

/// The longest this will sit waiting for a host's interval before giving up on the call.
///
/// An MCP tool call that blocks for a minute looks to the calling model exactly like one that
/// hung. Past this, [`ResearchError::RateLimited`] says how long to wait and hands the decision
/// back — which is a thing a model can act on, and a stall is not.
pub const MAX_PACING_WAIT: Duration = Duration::from_secs(5);

/// How long a parsed `robots.txt` is trusted before it is fetched again.
pub const ROBOTS_TTL: Duration = Duration::from_secs(30 * 60);

/// How many hops a redirect chain may take before it is called a loop.
pub const MAX_REDIRECTS: usize = 10;

/// The whole-exchange timeout for one request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// The search endpoint used when the user has configured none.
///
/// **DuckDuckGo's HTML interface, and the choice was measured rather than assumed.** Both
/// halves were checked against the live hosts on 2026-08-13:
///
/// - `https://html.duckduckgo.com/robots.txt` reads, in full, `User-agent: *` / `Allow: /` —
///   the host that serves this interface permits every path to every agent. (The main
///   `duckduckgo.com` host is a different matter: it carries `Disallow: /html` and
///   `Disallow: /lite`. This is not that host, and the distinction is the entire reason this
///   endpoint is usable at all.)
/// - A plain `GET …/html/?q=…` with [`DEFAULT_USER_AGENT`] answered **200** with **10**
///   results.
///
/// The two obvious alternatives were rejected on the same evidence. **Wikipedia's search API**
/// is disallowed to us: `en.wikipedia.org/robots.txt` carries `Disallow: /w/` and
/// `Disallow: /api/`, which covers `/w/api.php?action=query` outright — so using it would mean
/// either breaking the policy this module states three paragraphs above, or writing an
/// exemption for APIs, which is the same thing with a nicer name. **Brave, Bing, Google and
/// Mojeek** all need an API key, and there is none here.
///
/// A robots.txt is a live document and this one may change. That is why the check is *made on
/// every search* rather than assumed from this comment: if DuckDuckGo starts refusing us, the
/// search returns [`ResearchError::RobotsDisallowed`] naming the endpoint, which is the honest
/// answer and a legible instruction to configure another one.
pub const DEFAULT_SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// The environment variable that points search somewhere else.
///
/// Set it to a SearXNG instance's search URL — the user's own, ideally — and results come from
/// there instead. Set it to an empty string, or to `off`, and search is disabled with a named
/// refusal rather than a silent nothing.
pub const SEARCH_ENDPOINT_ENV: &str = "VELM_SEARCH_ENDPOINT";

/// The environment variable that lifts the host policy — [`ResearchConfig::allow_local_hosts`].
///
/// **It exists because the refusal advertises it.** [`ResearchError::BlockedHost`]'s message
/// tells the user to turn `allow_local_hosts` on if the address really is their own server, and
/// for as long as that field was written in exactly one place — `Default::default()`, as
/// `false` — the only way to take that advice was to edit Rust and rebuild. An escape hatch
/// nobody can reach is worse than none: it reads as a setting that has been looked for and not
/// found.
///
/// Set to `1`, `true`, `yes` or `on`. Anything else, including unset, leaves the policy on.
///
/// **An environment variable rather than a menu**, matching [`SEARCH_ENDPOINT_ENV`]: both are
/// switches a *user* sets deliberately for a whole session, and neither is a thing a model can
/// reach — nothing on the canvas writes them, and this is read once when the tool is built.
pub const ALLOW_LOCAL_HOSTS_ENV: &str = "VELM_ALLOW_LOCAL_HOSTS";

/// The environment variable that says what shape the configured endpoint answers in.
///
/// `json` (the default for a *configured* endpoint, because a user pointing Velm at their own
/// search is nearly always pointing it at a SearXNG JSON API) or `html` (the DuckDuckGo HTML
/// shape). An unset endpoint always uses `html`, since [`DEFAULT_SEARCH_ENDPOINT`] is that.
pub const SEARCH_FORMAT_ENV: &str = "VELM_SEARCH_FORMAT";

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// Why a fetch or a search produced nothing.
///
/// **A module-local error type, against this crate's own "one error type" rule, and
/// deliberately.** [`crate::AgentError`] flattens everything to a sentence because everything
/// it describes ends up in a toast. These do not: a research failure is read by *a model*,
/// which will pick a different source given a status code and will retry pointlessly given
/// prose. The status code and the disallowing rule are the payload, so they are fields.
/// [`From<ResearchError>`] puts one back into the crate-wide type at the boundary.
#[derive(Debug, thiserror::Error)]
pub enum ResearchError {
    /// Not an `http`/`https` URL. A refusal rather than a preference: without it a URL from a
    /// board file could name a local path and this would read it.
    #[error("`{0}` is not an http(s) URL")]
    NotHttp(String),

    /// The server answered, and its answer was no. **The status is the useful part** — 403 and
    /// 404 mean different things to whoever reads this next.
    #[error("{url} answered {status}")]
    Refused { url: String, status: u16 },

    /// The site's `robots.txt` disallows this path for us.
    #[error("{url} is disallowed by {host}/robots.txt (the rule was `{rule}`)")]
    RobotsDisallowed { url: String, host: String, rule: String },

    /// The host is on this machine or on the local network. See the module's host policy.
    ///
    /// **Its own variant, and not a [`ResearchError::Network`]**, because the two say opposite
    /// things to the thing that reads them. A network failure is worth retrying and a policy
    /// refusal never is — an agent handed *"could not reach it"* tries again, and again, at
    /// whatever address it was told not to reach.
    #[error(
        "{url} was not fetched: {host} is {reason}, and Velm's research tool reaches the \
         public internet only. Use a public address, or ask the user to set \
         VELM_ALLOW_LOCAL_HOSTS=1 if this really is their own server."
    )]
    BlockedHost { url: String, host: String, reason: String },

    /// The exchange failed below HTTP: DNS, TLS, a dropped connection, a timeout.
    #[error("could not reach {url}: {message}")]
    Network { url: String, message: String },

    /// The body could not be read to the cap.
    #[error("reading {url} failed: {message}")]
    Body { url: String, message: String },

    /// This host's pacing interval is longer than a tool call should block for.
    #[error("{host} is being paced; try again in about {seconds}s")]
    RateLimited { host: String, seconds: u64 },

    /// A redirect chain that never arrived. See [`MAX_REDIRECTS`] for the limit.
    #[error("{url} redirected too many times")]
    TooManyRedirects { url: String },

    /// Search is switched off, or has nowhere to go.
    #[error("{0}")]
    NoSearchEngine(String),

    /// The endpoint answered and nothing could be read out of the answer.
    ///
    /// Its own variant so a search can **never** degrade to a silent empty list. An agent told
    /// *"no results"* stops looking; an agent told *"the endpoint answered 200 and this build
    /// could not read its results"* asks again or asks elsewhere.
    #[error("{endpoint} answered, but no results could be read from it: {message}")]
    SearchUnreadable { endpoint: String, message: String },
}

impl From<ResearchError> for crate::AgentError {
    fn from(error: ResearchError) -> Self {
        match error {
            ResearchError::Network { url, message } | ResearchError::Body { url, message } => {
                Self::Transport { transport: "research", message: format!("{url}: {message}") }
            }
            other => Self::Refused(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------

/// Which search backend, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEngine {
    /// A page of DuckDuckGo-shaped HTML results, parsed by [`parse_html_results`].
    Html { endpoint: String },
    /// A SearXNG-shaped JSON document, parsed by [`parse_searxng_results`].
    Json { endpoint: String },
    /// Switched off by configuration. Answers a named refusal.
    Off,
}

impl Default for SearchEngine {
    fn default() -> Self {
        Self::Html { endpoint: DEFAULT_SEARCH_ENDPOINT.to_owned() }
    }
}

impl SearchEngine {
    /// Reads [`SEARCH_ENDPOINT_ENV`] and [`SEARCH_FORMAT_ENV`].
    ///
    /// Read once when a [`Research`] is built rather than per call, so a search cannot change
    /// destination halfway through a session. [`ResearchConfig::from_env`] reads the other one
    /// ([`ALLOW_LOCAL_HOSTS_ENV`]) beside it; this used to be the only environment read in the
    /// module, which is exactly why the host policy's own switch had no way of being set.
    pub fn from_env() -> Self {
        let endpoint = std::env::var(SEARCH_ENDPOINT_ENV).unwrap_or_default();
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Self::default();
        }
        if endpoint.eq_ignore_ascii_case("off") || endpoint.eq_ignore_ascii_case("none") {
            return Self::Off;
        }
        let format = std::env::var(SEARCH_FORMAT_ENV).unwrap_or_default();
        if format.trim().eq_ignore_ascii_case("html") {
            Self::Html { endpoint: endpoint.to_owned() }
        } else {
            Self::Json { endpoint: endpoint.to_owned() }
        }
    }

    /// The endpoint, for an error message that names where the failure was.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Html { endpoint } | Self::Json { endpoint } => Some(endpoint.as_str()),
            Self::Off => None,
        }
    }
}

/// The knobs, with defaults sized for an agent reading a few pages.
#[derive(Debug, Clone)]
pub struct ResearchConfig {
    pub user_agent: String,
    pub timeout: Duration,
    pub max_bytes: usize,
    pub max_text_chars: usize,
    pub min_interval: Duration,
    /// Whether `robots.txt` is consulted at all.
    ///
    /// It exists as a field so the *tests* can build a `Research` that makes no request, and
    /// so a future local-network mode has somewhere to say so. **It is `true` in every
    /// constructor here and nothing in the shipping path sets it false** — a switch that
    /// turned the policy off would be the evasion this module refuses to be.
    pub respect_robots: bool,

    /// Whether the host policy is lifted. See the module header.
    ///
    /// **Off by default, and set by the user through [`ALLOW_LOCAL_HOSTS_ENV`] — which is the
    /// part that used to be missing.** It was `false` in the only place that wrote it, so the
    /// sentence below ("a policy the user cannot override on their own machine") described the
    /// build rather than the thing it was arguing against. [`ResearchConfig::from_env`] is what
    /// makes it reachable, and it is the constructor the running tool uses. A
    /// user pointing an agent at their own local model, their own wiki or their own SearXNG is
    /// a real thing to want, and refusing it outright would be a policy the user cannot
    /// override on their own machine. What it must never be is the *default*, because the URL
    /// this guards is chosen by a model and the addresses behind it — cloud metadata, the
    /// router's admin page, Velm's own IPC server — are reachable with no credential at all.
    ///
    /// It is one flag rather than an allow-list of hosts on purpose: an allow-list is a thing
    /// a model can talk a user into extending one entry at a time, and a single switch is a
    /// decision somebody makes once, knowingly.
    pub allow_local_hosts: bool,

    pub search: SearchEngine,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
            max_text_chars: DEFAULT_MAX_TEXT_CHARS,
            min_interval: DEFAULT_MIN_INTERVAL,
            respect_robots: true,
            allow_local_hosts: false,
            search: SearchEngine::default(),
        }
    }
}

impl ResearchConfig {
    /// The defaults, with the two things the **user** is allowed to change read from the
    /// environment: where search goes, and whether the host policy is lifted.
    ///
    /// This is what the running tool is built from. The two fields are together here because
    /// they are the same kind of thing — a decision the person at the keyboard makes for a
    /// session — and because the alternative was what shipped: `allow_local_hosts` written in
    /// one place, as `false`, while the refusal it guards told the user to turn it on.
    pub fn from_env() -> Self {
        Self {
            search: SearchEngine::from_env(),
            allow_local_hosts: env_flag(ALLOW_LOCAL_HOSTS_ENV),
            ..Self::default()
        }
    }
}

/// One environment variable read as a switch.
///
/// Deliberately narrow: the four spellings someone actually types, and **everything else is
/// off**. A flag that treats any non-empty value as `true` turns `VELM_ALLOW_LOCAL_HOSTS=no`
/// into permission, and the value of this particular switch is that it is hard to turn on by
/// accident.
fn env_flag(name: &str) -> bool {
    flag_is_on(std::env::var(name).ok().as_deref())
}

/// The reading half, split out so it can be asserted **without setting an environment
/// variable**: a test binary runs its tests as threads in one process, so a test that writes
/// the environment writes it for every other test running at that moment.
fn flag_is_on(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    let value = value.trim();
    ["1", "true", "yes", "on"].iter().any(|form| value.eq_ignore_ascii_case(form))
}

// ---------------------------------------------------------------------------------------
// What comes back
// ---------------------------------------------------------------------------------------

/// A page, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The URL actually read, after redirects — not the one asked for. An agent that followed
    /// a link and got somewhere else needs to know where it ended up.
    pub url: String,
    pub status: u16,
    pub title: Option<String>,
    /// The readable text, as close to markdown as a tolerant extractor can get: headings kept,
    /// list items bulleted, links written `[text](target)`.
    pub text: String,
    /// Whether [`ResearchConfig::max_bytes`] or [`ResearchConfig::max_text_chars`] cut it
    /// short. Reported rather than hidden — an agent that read half an article and did not
    /// know it is worse than one that has to ask again.
    pub truncated: bool,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

// ---------------------------------------------------------------------------------------
// robots.txt
// ---------------------------------------------------------------------------------------

/// One `Allow:` or `Disallow:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    pattern: String,
}

/// The rules that apply to *us*, from one site's `robots.txt`.
///
/// Parsed rather than pattern-matched loosely, because the two things that go wrong here both
/// go wrong silently: a group written for another agent applied to us refuses paths we may
/// read, and a group of ours missed allows paths we may not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Robots {
    rules: Vec<Rule>,
    /// Seconds, as published. Held as milliseconds so a fractional delay survives.
    crawl_delay_ms: Option<u64>,
}

impl Robots {
    /// A site with no `robots.txt`, or one we could not read. Everything is permitted.
    ///
    /// **This is also what an unreachable or non-200 `robots.txt` becomes**, and the choice is
    /// deliberate: a site that publishes no rules is permitting everything, and treating a DNS
    /// blip or a 500 as a prohibition would make research fail in a way no user could diagnose.
    /// The stricter convention — treat a 5xx as *disallow all* — is a crawler's rule, written
    /// for a process making millions of requests unattended. This one makes a handful, on a
    /// person's instruction, and the cost of being wrong is not symmetric.
    pub fn permissive() -> Self {
        Self::default()
    }

    /// Parses a `robots.txt` for the group that applies to `token`.
    ///
    /// The rules implemented, and they are the ones that matter:
    ///
    /// - Groups are `User-agent:` lines followed by rules. **Consecutive** user-agent lines
    ///   share one group; a user-agent line after a rule starts a new one.
    /// - A group naming our token wins outright over the `*` group — even when it is *empty*,
    ///   which is how a site says *"Velm may read everything"* while restricting others.
    /// - `Allow` and `Disallow` patterns support `*` (any run) and a trailing `$` (end anchor).
    /// - The **longest** matching pattern decides; on a tie, `Allow` wins. That is Google's
    ///   documented precedence and the one nearly every site is written against.
    /// - An **empty** `Disallow:` is not a rule that matches nothing-in-particular — it is the
    ///   spelling of *"no restriction"*, so it is dropped rather than stored.
    /// - Anything unrecognised is ignored. A malformed file must not be able to fail a fetch.
    pub fn parse(text: &str, token: &str) -> Self {
        let token = token.to_ascii_lowercase();

        // Collected separately, because a group for us beats the wildcard group whether or not
        // it has any rules in it.
        let mut ours: Option<Robots> = None;
        let mut wildcard: Option<Robots> = None;

        // The group being read: which agents it names, and whether a rule has been seen since
        // the last `User-agent:` line (which is what ends a run of agent names).
        let mut agents: Vec<String> = Vec::new();
        let mut group = Robots::default();
        let mut seen_rule = false;

        // ⚠ The reset at the end runs **even when no agent was named**, and that is the whole
        // point of it: a rule appearing before any `User-agent:` line belongs to nobody, and
        // an early return here would leave it in `group` to be inherited by the next group
        // that *is* named. A malformed file would then hand one site's stray `Disallow:` to
        // everybody.
        let mut flush = |agents: &mut Vec<String>, group: &mut Robots| {
            for agent in agents.iter() {
                if agent == &token {
                    merge(&mut ours, group);
                } else if agent == "*" {
                    merge(&mut wildcard, group);
                }
            }
            agents.clear();
            *group = Robots::default();
        };

        for raw in text.lines() {
            // A `#` starts a comment anywhere on the line.
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue; // Not a directive at all. Ignored, never fatal.
            };
            let field = field.trim().to_ascii_lowercase();
            let value = value.trim();

            match field.as_str() {
                "user-agent" => {
                    if seen_rule {
                        flush(&mut agents, &mut group);
                        seen_rule = false;
                    }
                    agents.push(value.to_ascii_lowercase());
                }
                "allow" | "disallow" => {
                    seen_rule = true;
                    // An empty `Disallow:` means "nothing is disallowed" — it is the absence of
                    // a rule, not a rule matching the empty prefix (which would match every
                    // path there is and lock the whole site).
                    if !value.is_empty() {
                        group.rules.push(Rule {
                            allow: field == "allow",
                            pattern: value.to_owned(),
                        });
                    }
                }
                "crawl-delay" => {
                    seen_rule = true;
                    if let Ok(seconds) = value.parse::<f64>()
                        && seconds.is_finite()
                        && seconds > 0.0
                    {
                        // Clamped: a site publishing `Crawl-delay: 86400` is not going to be
                        // waited out inside a tool call, and the pacing wait has its own
                        // ceiling anyway.
                        let ms = (seconds * 1000.0).min(60_000.0) as u64;
                        group.crawl_delay_ms = Some(ms);
                    }
                }
                _ => {}
            }
        }
        flush(&mut agents, &mut group);

        ours.or(wildcard).unwrap_or_default()
    }

    /// Whether we may fetch this path, and the rule that decided it when the answer is no.
    ///
    /// `path` is the path **and query**, as sent on the wire — `robots.txt` patterns are
    /// routinely written against a query string (`Disallow: /*?`), so dropping it would quietly
    /// allow exactly the URLs a site meant to refuse.
    pub fn allows(&self, path: &str) -> Result<(), String> {
        let path = if path.starts_with('/') { path } else { "/" };
        let mut best: Option<&Rule> = None;
        for rule in &self.rules {
            if !glob_matches(&rule.pattern, path) {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    let (a, b) = (specificity(&rule.pattern), specificity(&current.pattern));
                    // Longest wins; on a tie, Allow wins. The tie rule is the load-bearing
                    // half: `Allow: /w/api.php` against `Disallow: /w/api.php` is how a site
                    // carves one endpoint out of a blanket refusal.
                    a > b || (a == b && rule.allow && !current.allow)
                }
            };
            if better {
                best = Some(rule);
            }
        }
        match best {
            Some(rule) if !rule.allow => Err(format!("Disallow: {}", rule.pattern)),
            _ => Ok(()),
        }
    }

    /// The published crawl delay, if any.
    pub fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay_ms.map(Duration::from_millis)
    }
}

/// Folds one parsed group into the accumulator for an agent named more than once.
fn merge(slot: &mut Option<Robots>, group: &Robots) {
    match slot {
        Some(existing) => {
            existing.rules.extend(group.rules.iter().cloned());
            existing.crawl_delay_ms = existing.crawl_delay_ms.or(group.crawl_delay_ms);
        }
        None => *slot = Some(group.clone()),
    }
}

/// How specific a pattern is, for the longest-match rule.
///
/// The `$` anchor is not part of the path it matches, so it does not count towards length —
/// otherwise `Disallow: /a$` would beat `Allow: /a/b` on a two-character difference that says
/// nothing about specificity.
fn specificity(pattern: &str) -> usize {
    pattern.strip_suffix('$').unwrap_or(pattern).chars().count()
}

/// `robots.txt` globbing: `*` matches any run, a trailing `$` anchors the end, everything else
/// is a literal prefix match.
///
/// Iterative rather than recursive — a pattern of forty `*`s from a hostile `robots.txt` must
/// not be able to blow the stack, and `panic = "abort"` in the release profile means there is
/// no catching it if it does.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(rest) => (rest, true),
        None => (pattern, false),
    };

    let segments: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;

    for (index, segment) in segments.iter().enumerate() {
        let first = index == 0;
        let last = index + 1 == segments.len();

        if segment.is_empty() {
            // A leading, trailing or doubled `*`. Nothing to place.
            if last && anchored {
                return true; // `…*$` — the run reaches the end by construction.
            }
            continue;
        }
        let rest = match path.get(cursor..) {
            Some(rest) => rest,
            None => return false,
        };
        if first {
            // The pattern is a prefix match, so the first literal must sit at the start.
            if !rest.starts_with(segment) {
                return false;
            }
            cursor += segment.len();
        } else if last && anchored {
            return rest.ends_with(segment);
        } else if let Some(at) = rest.find(segment) {
            cursor += at + segment.len();
        } else {
            return false;
        }
    }

    if anchored {
        // No trailing wildcard: the whole path must have been consumed.
        cursor == path.len()
    } else {
        true
    }
}

// ---------------------------------------------------------------------------------------
// Pacing
// ---------------------------------------------------------------------------------------

/// A minimum interval between requests to one host.
///
/// Pure. `now` is a parameter on every method, which is what makes *"has this host's interval
/// elapsed"* a unit test rather than a `sleep` — and it is the crate-wide rule stated in
/// `lib.rs`, applied here.
#[derive(Debug, Default)]
pub struct RateLimiter {
    last: HashMap<String, Instant>,
    /// Per-host overrides, from a published `Crawl-delay`.
    interval: HashMap<String, Duration>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises this host's interval. **Raises only**: a site is allowed to ask us to slow down
    /// and is not allowed to ask us to speed up past the floor.
    pub fn set_interval(&mut self, host: &str, interval: Duration) {
        let entry = self.interval.entry(host.to_owned()).or_insert(interval);
        *entry = (*entry).max(interval);
    }

    /// How long to wait before touching this host again. Zero when it is free.
    pub fn delay_before(&self, host: &str, floor: Duration, now: Instant) -> Duration {
        let Some(last) = self.last.get(host) else {
            return Duration::ZERO;
        };
        let required = self.interval.get(host).copied().unwrap_or(floor).max(floor);
        let elapsed = now.saturating_duration_since(*last);
        required.saturating_sub(elapsed)
    }

    /// Records that a request to this host is being made at `now`.
    pub fn note(&mut self, host: &str, now: Instant) {
        self.last.insert(host.to_owned(), now);
    }
}

// ---------------------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    /// Without a leading dot. A host-only cookie stores the exact host.
    domain: String,
    path: String,
    secure: bool,
    host_only: bool,
}

/// A session cookie jar.
///
/// Hand-rolled, because this build of `ureq` is compiled `default-features = false` with only
/// `rustls` — the `cookies` feature is off and [`ureq::CookieJar`] does not exist in the
/// binary. That is checked, not assumed: `cookie_store` is absent from `Cargo.lock`.
///
/// # What it does not do, stated rather than discovered
///
/// - **No public-suffix list.** A cookie's `Domain` must contain a dot and must be a suffix of
///   the request host, which stops `Domain=.com` and stops a cross-site set outright, and does
///   *not* stop a co.uk-shaped registry from being written to. Nothing here logs in to
///   anything, so the exposure is a shared session on a domain we were already talking to.
/// - **No `SameSite`.** It exists to defend a browser against a *third party's* page issuing a
///   request; there is no third party here, only the one URL the agent asked for.
/// - **No expiry, and no disk.** The jar is a session — it lives as long as the [`Research`]
///   that owns it. A cookie jar that outlived the process would be a tracking identifier we
///   had chosen to keep.
#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// The most cookies held for one domain, so a hostile `Set-Cookie` loop cannot grow this
    /// without bound.
    const PER_DOMAIN: usize = 32;
    /// And a ceiling across the jar.
    const TOTAL: usize = 512;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Files one `Set-Cookie` header value.
    ///
    /// Tolerant by design: an attribute this does not understand is skipped rather than
    /// failing the cookie, because a cookie dropped for an unknown attribute is a session that
    /// silently does not persist.
    pub fn store(&mut self, header: &str, host: &str, request_path: &str) {
        let mut parts = header.split(';');
        let Some(pair) = parts.next() else { return };
        let Some((name, value)) = pair.split_once('=') else { return };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            return;
        }

        let mut domain = host.to_ascii_lowercase();
        let mut host_only = true;
        let mut path = default_cookie_path(request_path);
        let mut secure = false;

        for attribute in parts {
            let attribute = attribute.trim();
            let (key, argument) = match attribute.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
                None => (attribute.to_ascii_lowercase(), ""),
            };
            match key.as_str() {
                "domain" => {
                    let candidate = argument.trim_start_matches('.').to_ascii_lowercase();
                    if domain_is_acceptable(&candidate, host) {
                        domain = candidate;
                        host_only = false;
                    }
                    // An unacceptable Domain leaves the cookie host-only, which is what a
                    // browser does. Dropping it entirely would break sites that set a
                    // redundant `Domain` on their own host.
                }
                "path" if argument.starts_with('/') => path = argument.to_owned(),
                "secure" => secure = true,
                _ => {}
            }
        }

        let cookie =
            Cookie { name: name.to_owned(), value: value.to_owned(), domain, path, secure, host_only };

        // Same name, domain and path replaces rather than accumulates — that is the identity a
        // cookie has, and appending would make a session token grow a history.
        if let Some(slot) = self.cookies.iter_mut().find(|held| {
            held.name == cookie.name && held.domain == cookie.domain && held.path == cookie.path
        }) {
            *slot = cookie;
            return;
        }

        let held_here = self.cookies.iter().filter(|held| held.domain == cookie.domain).count();
        if held_here >= Self::PER_DOMAIN || self.cookies.len() >= Self::TOTAL {
            return;
        }
        self.cookies.push(cookie);
    }

    /// The `Cookie:` header for a request, or `None` when nothing matches.
    pub fn header_for(&self, host: &str, path: &str, https: bool) -> Option<String> {
        let host = host.to_ascii_lowercase();
        let mut matched: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| {
                if cookie.secure && !https {
                    return false;
                }
                let domain_ok = if cookie.host_only {
                    host == cookie.domain
                } else {
                    host_matches_domain(&host, &cookie.domain)
                };
                domain_ok && path_matches(path, &cookie.path)
            })
            .collect();
        if matched.is_empty() {
            return None;
        }
        // Longest path first, which is the order RFC 6265 asks for and the order servers that
        // read only the first occurrence of a name are written against.
        matched.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        Some(
            matched
                .iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// A `Domain` attribute we are willing to widen a cookie to.
///
/// Must contain a dot — which refuses `Domain=com` — and the request host must be it or a
/// subdomain of it, which refuses a cross-site set. See [`CookieJar`] for what this
/// deliberately does not cover.
fn domain_is_acceptable(domain: &str, host: &str) -> bool {
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }
    host_matches_domain(&host.to_ascii_lowercase(), domain)
}

fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// RFC 6265's default-path: everything up to the last `/`, or `/`.
fn default_cookie_path(request_path: &str) -> String {
    let path = request_path.split('?').next().unwrap_or("/");
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(at) => path.get(..at).unwrap_or("/").to_owned(),
    }
}

/// RFC 6265's path-match: equal, or a prefix ending at a `/` boundary.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    let path = request_path.split('?').next().unwrap_or("/");
    if path == cookie_path {
        return true;
    }
    if !path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || path.get(cookie_path.len()..).is_some_and(|r| r.starts_with('/'))
}

// ---------------------------------------------------------------------------------------
// URLs, without the `url` crate
// ---------------------------------------------------------------------------------------

/// The pieces of an http(s) URL this module needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    /// Host **and port**, as it appears in the authority and as `robots.txt` and the cookie jar
    /// key off it.
    pub host: String,
    /// Path and query, beginning with `/`.
    pub path: String,
}

impl UrlParts {
    pub fn is_https(&self) -> bool {
        self.scheme == "https"
    }

    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.host)
    }
}

/// Splits an http(s) URL. `None` for anything else — including `file:` and `mailto:`, which is
/// the refusal that stops a URL from a board file naming a local path.
///
/// Hand-rolled because `url` is not a dependency of this crate and rule 5 of this crate's brief
/// forbids adding one. It handles scheme, optional `userinfo@`, host, port, path, query and
/// fragment, and it does **not** do IDNA, normalisation or percent-encoding of the path.
pub fn split_url(url: &str) -> Option<UrlParts> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    // The fragment is never sent, so it is dropped here rather than at the request.
    let rest = rest.split('#').next().unwrap_or("");
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => (rest.get(..at).unwrap_or(""), rest.get(at..).unwrap_or("/")),
        None => (rest, "/"),
    };
    // `user:pass@host` — dropped, because nothing here authenticates and carrying credentials
    // into a redirect chain is how they end up on somebody else's server.
    let host = authority.rsplit('@').next().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
    Some(UrlParts { scheme, host, path })
}

// ---------------------------------------------------------------------------------------
// The host policy
// ---------------------------------------------------------------------------------------

/// The bare host out of an authority: no port, no IPv6 brackets.
///
/// [`UrlParts::host`] is host **and** port, because that is what `robots.txt` and the cookie
/// jar key off. The policy needs the other thing.
///
/// The bracket case is not a nicety: an IPv6 literal is full of colons, which is the reason
/// the brackets exist, and splitting on the last colon without handling them turns `[::1]`
/// into `[:` — a name that is refused for the wrong reason today and might not be tomorrow.
pub fn hostname_of(authority: &str) -> &str {
    let authority = authority.trim();
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    // A bare, unbracketed IPv6 literal is not legal in a URL, but it is what a hand-written
    // string contains often enough to matter, and the port split would mangle it.
    if authority.matches(':').count() > 1 {
        return authority;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) => {
            host
        }
        _ => authority,
    }
}

/// Whether this path **is** `/robots.txt`, the one path exempt from `robots.txt`.
///
/// ⚠ **Equality, not a prefix.** The exemption exists because fetching the rules cannot be
/// subject to them, and `starts_with("/robots.txt")` said the same thing about
/// `/robots.txt.bak`, `/robots.txt/../private/secret` and `/robots.txtsecret` — three paths
/// that are not the rules file, each of which then skipped the robots check for the whole
/// request. A prefix test on a path is an access-control hole in any module that has one.
///
/// The query and fragment come off first. [`UrlParts::path`] carries the query, so
/// `/robots.txt?v=2` is the rules file and must stay exempt; the fragment is already dropped
/// by [`split_url`] and is handled here anyway, so this does not depend on that staying true.
pub fn is_robots_path(path: &str) -> bool {
    path.split(['?', '#']).next().unwrap_or_default() == "/robots.txt"
}

/// Why this IPv4 address is not on the public internet, if it is not.
fn blocked_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [first, second, ..] = ip.octets();
    if ip.is_loopback() {
        return Some("a loopback address (127.0.0.0/8) — this machine");
    }
    if ip.is_private() {
        return Some("a private address (10/8, 172.16/12 or 192.168/16) — the local network");
    }
    if ip.is_link_local() {
        return Some(
            "a link-local address (169.254.0.0/16) — where cloud instance metadata, and the \
             credentials it hands out, live",
        );
    }
    if ip.is_broadcast() {
        return Some("the broadcast address");
    }
    // `is_shared` is still unstable, so carrier-grade NAT is spelled out. `0.0.0.0/8` covers
    // `is_unspecified` and the rest of "this network", which resolves to the local host on
    // most stacks.
    if first == 100 && (64..=127).contains(&second) {
        return Some("a carrier-grade NAT address (100.64.0.0/10)");
    }
    if first == 0 {
        return Some("a `this network` address (0.0.0.0/8) — this machine");
    }
    None
}

/// Why this address is not on the public internet, if it is not.
///
/// **The IPv4-mapped case is checked first and it is the one a policy usually forgets**:
/// `::ffff:127.0.0.1` is a perfectly ordinary way to spell the loopback address, and a v6
/// branch that only looks at `is_loopback` says yes to it.
pub fn blocked_ip(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_v4(v4);
            }
            if v6.is_loopback() {
                return Some("the IPv6 loopback address (::1) — this machine");
            }
            if v6.is_unspecified() {
                return Some("the unspecified address (::)");
            }
            let segments = v6.segments();
            // `is_unicast_link_local` and `is_unique_local` are both still unstable.
            if segments[0] & 0xffc0 == 0xfe80 {
                return Some("an IPv6 link-local address (fe80::/10)");
            }
            if segments[0] & 0xfe00 == 0xfc00 {
                return Some("an IPv6 unique-local address (fc00::/7) — the local network");
            }
            None
        }
    }
}

/// Why this **name** is not on the public internet, if it can be told without resolving it.
///
/// The cheap half of the policy, and the half that still works when there is no DNS: a literal
/// address needs no lookup, and the four name suffixes below cannot mean anything but a local
/// machine however they resolve. `.internal` matters more than it looks —
/// `metadata.google.internal` is the same instance-metadata endpoint `169.254.169.254` is,
/// reached by name.
pub fn blocked_by_name(hostname: &str) -> Option<&'static str> {
    let name = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return Some("an empty host");
    }
    if let Ok(ip) = name.parse::<IpAddr>() {
        return blocked_ip(ip);
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return Some("`localhost` — this machine");
    }
    if name == "local" || name.ends_with(".local") {
        return Some("a `.local` (mDNS) name, which only ever resolves on the local network");
    }
    if name.ends_with(".internal") || name == "internal" {
        return Some("an `.internal` name, which is a private-network or cloud-metadata name");
    }
    if name.ends_with(".home.arpa") || name == "home.arpa" {
        return Some("a `home.arpa` name, which is reserved for home networks");
    }
    None
}

/// Resolves `href` against the page it was found on.
///
/// Handles the four forms that matter: absolute (`https://…`), protocol-relative (`//host/…`),
/// root-relative (`/path`) and plain relative (`sibling.html`, `../up`). It does **not**
/// implement `<base href>`, and it does not normalise `.`/`..` beyond popping a segment —
/// stated because a link rewritten wrongly is worse than a link left alone.
pub fn resolve_url(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    if href.contains("://") {
        return split_url(href).map(|_| href.to_owned());
    }
    let parts = split_url(base)?;
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{}://{rest}", parts.scheme));
    }
    if href.starts_with('/') {
        return Some(format!("{}{href}", parts.origin()));
    }
    // A scheme we do not follow — `mailto:`, `javascript:`, `tel:` — is not a relative path.
    // Resolving one would produce `https://example.com/mailto:a@b`, a link to nowhere that
    // looks exactly like a link to somewhere.
    if let Some(colon) = href.find(':') {
        let scheme = href.get(..colon).unwrap_or("");
        let is_a_scheme = scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && scheme.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'));
        if is_a_scheme {
            return None;
        }
    }

    let directory = parts.path.split('?').next().unwrap_or("/");
    let mut segments: Vec<&str> = directory.split('/').collect();
    segments.pop(); // the file name, or the empty tail of a trailing slash
    for segment in href.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                if segments.len() > 1 {
                    segments.pop();
                }
            }
            other => segments.push(other),
        }
    }
    Some(format!("{}{}", parts.origin(), segments.join("/")))
}

/// Percent-decodes, byte-wise, then interprets the result as UTF-8.
///
/// Byte-wise is the only correct order: a percent-escaped multi-byte character arrives as two
/// or more separate `%XX` groups, so decoding to `char`s one escape at a time mangles it. An
/// invalid sequence becomes U+FFFD rather than failing — a search result URL that is 99%
/// readable is worth more than none.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(high) = bytes.get(i + 1).and_then(|b| hex_value(*b))
            && let Some(low) = bytes.get(i + 2).and_then(|b| hex_value(*b))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes a query parameter value. Space becomes `+`, which every search endpoint
/// this talks to accepts and which is what a browser's form submission sends.
pub fn percent_encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// HTML entities
// ---------------------------------------------------------------------------------------

/// The named entities worth carrying, beyond the numeric forms.
///
/// Deliberately short. A full HTML5 table is 2,231 names, nearly all of which no page in a
/// research corpus emits by name, and the numeric branch below covers every character there is.
const NAMED_ENTITIES: &[(&str, char)] = &[
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", '\u{a0}'),
    ("ndash", '–'),
    ("mdash", '—'),
    ("hellip", '…'),
    ("lsquo", '\u{2018}'),
    ("rsquo", '\u{2019}'),
    ("ldquo", '\u{201c}'),
    ("rdquo", '\u{201d}'),
    ("laquo", '«'),
    ("raquo", '»'),
    ("middot", '·'),
    ("bull", '•'),
    ("times", '×'),
    ("divide", '÷'),
    ("deg", '°'),
    ("plusmn", '±'),
    ("copy", '©'),
    ("reg", '®'),
    ("trade", '™'),
    ("euro", '€'),
    ("pound", '£'),
    ("yen", '¥'),
    ("cent", '¢'),
    ("sect", '§'),
    ("para", '¶'),
    ("dagger", '†'),
    ("prime", '′'),
    ("rarr", '→'),
    ("larr", '←'),
    ("harr", '↔'),
    ("shy", '\u{ad}'),
    // `amp` is **absent on purpose**. See the doc comment on `decode_entities`.
];

/// Decodes HTML entities in **one left-to-right pass**.
///
/// # The trap this repo has already paid for twice
///
/// `vellum-link`'s first decoder was a sequence of `str::replace` calls with a comment saying
/// `&amp;` had to come last — and `&#38;` sat after it, so `&amp;#38;` decoded **twice**, to a
/// bare `&`. The importer's decoder then byte-sliced a fixed window and aborted the process on
/// a multi-byte character (feedback 30). Both are structural, so both are answered structurally
/// here rather than by ordering a table correctly:
///
/// - **Output is appended and never re-examined.** An `&` this function *produces* is not a
///   candidate for the next match, so `&amp;#38;` can only ever decode to `&#38;`. The bug is
///   not fixed, it is unrepresentable. That is why `amp` is not in [`NAMED_ENTITIES`] — it is
///   handled inline, in the same single pass, with the cursor advanced past what it wrote.
/// - **Scanning is by byte and slicing is by [`str::get`].** `&` and `;` are ASCII, and no
///   ASCII byte occurs inside a multi-byte UTF-8 sequence, so every index this produces is a
///   character boundary. `get` returning `Option` is the belt to that braces.
///
/// An `&` that begins nothing recognisable is emitted as itself — a page saying *"Tom & Jerry"*
/// is common and is not an error.
pub fn decode_entities(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'&' {
            // Copy the run up to the next `&` in one go. `find` returns a boundary, and the
            // slice starts at one, so this cannot split a character.
            let next = text.get(i..).and_then(|rest| rest.find('&')).map_or(bytes.len(), |at| i + at);
            out.push_str(text.get(i..next).unwrap_or(""));
            i = next;
            continue;
        }

        // An entity name is short. Bounding the search stops a lone `&` in a 2MB page from
        // scanning the rest of it looking for a `;` that is not there.
        const LONGEST: usize = 12;
        let window_end = (i + 1 + LONGEST).min(bytes.len());
        let window = text.get(i + 1..window_end).unwrap_or("");
        let Some(semicolon) = window.find(';') else {
            out.push('&');
            i += 1;
            continue;
        };
        let name = window.get(..semicolon).unwrap_or("");
        let after = i + 1 + semicolon + 1;

        let decoded = if let Some(digits) = name.strip_prefix('#') {
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse::<u32>().ok(),
            };
            code.and_then(char::from_u32)
        } else if name.eq_ignore_ascii_case("amp") {
            Some('&')
        } else {
            NAMED_ENTITIES
                .iter()
                .find(|(entity, _)| entity.eq_ignore_ascii_case(name))
                .map(|(_, ch)| *ch)
        };

        match decoded {
            Some(ch) => {
                out.push(ch);
                i = after;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// HTML → readable text
// ---------------------------------------------------------------------------------------

/// A page's title and its readable body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readable {
    pub title: Option<String>,
    pub text: String,
}

/// Elements whose *content* is thrown away along with their tags.
///
/// `script`/`style`/`noscript`/`template` are not prose at all. `nav`/`footer`/`aside`/`form`
/// are the boilerplate that makes an extracted page mostly menus, which is the difference
/// between a usable answer and 4,000 characters of site chrome. `svg` is markup that would
/// otherwise leak path data into the text.
const SKIPPED: &[&str] =
    &["script", "style", "noscript", "template", "svg", "iframe", "nav", "footer", "aside", "form"];

/// WAI-ARIA landmark roles that mean *this is the site, not the page*.
///
/// The tag list above only catches a site that uses the HTML5 elements; a great many mark
/// their chrome with a landmark role on a plain `<div>` instead.
///
/// **Measured, and the number is smaller than it first looked.** A/B'd on a real 240 KB
/// capture of `en.wikipedia.org/wiki/Torque_wrench`, same base URL both ways:
/// **43,074 characters by tag name alone, 42,535 with roles as well.** 539 characters, 1.2%.
/// What went was the search box, *Personal tools*, *24 languages* and the `v · t · e` navbox
/// links — chrome in every case, with no article text among it. So it earns its place on being
/// *right* rather than on being large, and the honest figure is recorded here because the
/// first version of this comment claimed 1,900 characters and a rewritten opening paragraph,
/// which the A/B did not support.
///
/// A landmark is the right signal because it is *the page telling us*, in a standard
/// vocabulary, rather than us guessing from a class name. `main` and `article` are absent for
/// the obvious reason. `dialog` was considered and left out: a modal is sometimes a cookie
/// banner and sometimes the only content there is.
const CHROME_ROLES: &[&str] =
    &["navigation", "banner", "contentinfo", "search", "menu", "menubar", "complementary"];

/// Elements that have no closing tag, and therefore may **never** be skipped as a container.
///
/// ⚠ This list is not tidiness, it is the fix for a measured catastrophe. `skip_element` walks
/// forward to `</name>` and, finding none, discards the rest of the document — which is the
/// right answer for an unterminated `<script>` on a page cut short by the byte cap, and is
/// ruinous for an element that never had a closing tag to begin with. Wikipedia's logo is
/// literally `<img class="mw-logo-icon" … aria-hidden="true">`; against a real 240 KB capture
/// it swallowed **226,863 bytes** and left **60 characters** of a 43,074-character extraction,
/// while every unit test stayed green — none of them had a void element carrying a landmark
/// attribute. It was found by running the extractor over a real page and looking at the
/// output, which is the only thing that could have found it.
///
/// It only became reachable when [`CHROME_ROLES`] did, because [`SKIPPED`] contains no void
/// element. That is the shape of it: a new *predicate* over the same old *action* reached a
/// case the action had never been asked about.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements that end a line.
const BREAKS: &[&str] = &[
    "p", "div", "section", "article", "main", "ul", "ol", "table", "thead", "tbody", "tr", "td",
    "th", "blockquote", "pre", "hr", "dl", "dt", "dd", "figure", "figcaption", "address",
];

/// Turns a page into readable text.
///
/// The result is markdown-shaped rather than markdown: headings get their `#`s, list items get
/// a `- `, paragraphs are separated by a blank line, and a link becomes `[text](target)` with
/// the target resolved against `base` when one is given.
///
/// # What it does not handle, so the next reader does not have to find out
///
/// - **No CSS and no JavaScript.** Content hidden by `display: none` is extracted, and content
///   inserted by a script is not there at all. A single-page application returns a shell.
/// - **No boilerplate scoring.** Readability-style extractors score blocks by text density;
///   this strips the elements listed in [`SKIPPED`] and keeps the rest. Simpler, and it never
///   throws away the article.
/// - **No table structure.** A table becomes its cells, one line each.
/// - **No `<base href>`.** Relative links resolve against the page's own URL.
/// - `>` **inside an attribute value is handled** (quotes are tracked while scanning for the
///   tag's end), because `alt="a > b"` is common enough to matter and would otherwise cut the
///   tag in half and spill markup into the text.
pub fn html_to_text(html: &str, base: Option<&str>) -> Readable {
    let bytes = html.as_bytes();
    let mut sink = Sink::default();
    let mut title: Option<String> = None;
    let mut link_target: Option<String> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let next =
                html.get(i..).and_then(|rest| rest.find('<')).map_or(bytes.len(), |at| i + at);
            sink.text(&decode_entities(html.get(i..next).unwrap_or("")));
            i = next;
            continue;
        }

        // A comment. Doctypes and processing instructions fall through to the generic tag scan,
        // which is enough for them because they contain no `>` we care about.
        if html.get(i..).is_some_and(|rest| rest.starts_with("<!--")) {
            i = match html.get(i + 4..).and_then(|rest| rest.find("-->")) {
                Some(at) => i + 4 + at + 3,
                None => bytes.len(),
            };
            continue;
        }

        let Some(end) = tag_end(bytes, i) else {
            // An unterminated `<` at the very end of the document. Not markup; not text worth
            // keeping either.
            break;
        };
        let source = html.get(i + 1..end).unwrap_or("");
        i = end + 1;

        let closing = source.starts_with('/');
        let name = tag_name(source);
        if name.is_empty() {
            continue;
        }

        if !closing
            && !source.ends_with('/')
            && !VOID.contains(&name.as_str())
            && (SKIPPED.contains(&name.as_str()) || is_site_chrome(source))
        {
            i = skip_element(html, bytes, i, &name);
            continue;
        }

        if name == "title" {
            if !closing {
                let (text, next) = raw_until_close(html, bytes, i, "title");
                title = Some(collapse(&decode_entities(&text)))
                    .filter(|found: &String| !found.is_empty());
                i = next;
            }
            continue;
        }

        match name.as_str() {
            "br" => sink.line_break(1),
            "li" => {
                if closing {
                    sink.line_break(1);
                } else {
                    sink.line_break(1);
                    sink.marker("- ");
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                sink.line_break(2);
                if !closing {
                    let level = name.get(1..).and_then(|d| d.parse::<usize>().ok()).unwrap_or(1);
                    sink.marker(&format!("{} ", "#".repeat(level)));
                }
            }
            "a" => {
                if closing {
                    if let Some(target) = link_target.take() {
                        sink.tight_marker(&format!("]({target})"));
                    }
                } else if let Some(href) = attribute(source, "href") {
                    let href = decode_entities(&href);
                    let resolved = match base {
                        Some(base) => resolve_url(base, &href),
                        None => href.contains("://").then(|| href.clone()),
                    };
                    if let Some(target) = resolved {
                        sink.marker("[");
                        link_target = Some(target);
                    }
                }
            }
            other if BREAKS.contains(&other) => sink.line_break(2),
            _ => {}
        }
    }

    // An `<a>` that never closed would otherwise leave a dangling `[`.
    if let Some(target) = link_target.take() {
        sink.tight_marker(&format!("]({target})"));
    }

    Readable { title, text: sink.finish() }
}

/// Whether an element declares itself to be site chrome rather than page content.
///
/// See [`CHROME_ROLES`] for the measurement that justifies this existing at all.
/// `aria-hidden="true"` joins it because an element hidden from a screen reader is by the
/// page's own account not part of what it says — icon fonts, decorative duplicates, the
/// off-canvas copy of a menu.
fn is_site_chrome(source: &str) -> bool {
    // A cheap gate before the two `attribute` calls, each of which lowercases the whole tag:
    // this runs on every opening tag of a page that may be 2 MiB, and the overwhelming
    // majority of them carry neither attribute.
    if find_ci(source, "role", 0).is_none() && find_ci(source, "aria-hidden", 0).is_none() {
        return false;
    }
    if let Some(role) = attribute(source, "role")
        && CHROME_ROLES.contains(&role.trim().to_ascii_lowercase().as_str())
    {
        return true;
    }
    attribute(source, "aria-hidden").is_some_and(|value| value.trim() == "true")
}

/// Finds the `>` that ends a tag, ignoring one inside a quoted attribute value.
///
/// Byte-wise, and safely so: `<`, `>`, `"` and `'` are ASCII and cannot occur inside a
/// multi-byte UTF-8 sequence, so every index returned is a character boundary.
fn tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut i = start + 1;
    while i < bytes.len() {
        let byte = bytes[i];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

/// The lowercase element name from a tag's source, with any leading `/` removed.
fn tag_name(source: &str) -> String {
    source
        .trim_start_matches('/')
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// One attribute's value, unquoted and not yet entity-decoded.
///
/// Requires the name to start at the tag or after whitespace, so `href` is not found inside
/// `data-href`.
fn attribute(source: &str, name: &str) -> Option<String> {
    let lower = source.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(at) = lower.get(from..).and_then(|rest| rest.find(name)) {
        let at = from + at;
        let before_ok = at == 0
            || lower
                .get(..at)
                .and_then(|s| s.chars().next_back())
                .is_some_and(char::is_whitespace);
        let after = lower.get(at + name.len()..).unwrap_or("");
        let after_trimmed = after.trim_start();
        if before_ok && after_trimmed.starts_with('=') {
            let value = source
                .get(at + name.len() + (after.len() - after_trimmed.len()) + 1..)
                .unwrap_or("")
                .trim_start();
            let mut chars = value.chars();
            return match chars.next() {
                Some(quote @ ('"' | '\'')) => {
                    let rest = value.get(1..).unwrap_or("");
                    let end = rest.find(quote).unwrap_or(rest.len());
                    Some(rest.get(..end).unwrap_or("").to_owned())
                }
                Some(_) => {
                    let end = value.find(char::is_whitespace).unwrap_or(value.len());
                    Some(value.get(..end).unwrap_or("").trim_end_matches('/').to_owned())
                }
                None => None,
            };
        }
        from = at + name.len();
    }
    None
}

/// Skips to just past `</name>`, or to the end of the document.
fn skip_element(html: &str, bytes: &[u8], from: usize, name: &str) -> usize {
    let needle = format!("</{name}");
    match find_ci(html, &needle, from) {
        Some(at) => tag_end(bytes, at).map_or(bytes.len(), |end| end + 1),
        None => bytes.len(),
    }
}

/// The raw text up to `</name>`, and the index just past that close tag.
fn raw_until_close(html: &str, bytes: &[u8], from: usize, name: &str) -> (String, usize) {
    let needle = format!("</{name}");
    match find_ci(html, &needle, from) {
        Some(at) => {
            let text = html.get(from..at).unwrap_or("").to_owned();
            let next = tag_end(bytes, at).map_or(bytes.len(), |end| end + 1);
            (text, next)
        }
        None => (html.get(from..).unwrap_or("").to_owned(), bytes.len()),
    }
}

/// Case-insensitive substring search from a byte offset, returning a byte offset.
///
/// The needle is ASCII in every caller, so folding case byte-wise is correct and the returned
/// index is a character boundary.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return None;
    }
    (from..=bytes.len().saturating_sub(needle.len()))
        .find(|&start| {
            bytes.get(start..start + needle.len()).is_some_and(|w| w.eq_ignore_ascii_case(needle))
        })
}

/// Collapses every run of whitespace to one space and trims.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Accumulates text with whitespace collapsed and line breaks requested rather than written.
///
/// Requested rather than written, because a run of `</p></div></section>` asks for a break
/// three times and must produce one. Holding the request until text actually arrives is also
/// what stops a page ending in eleven closing tags from ending in eleven blank lines.
#[derive(Default)]
struct Sink {
    out: String,
    breaks: usize,
    space: bool,
}

impl Sink {
    fn text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_whitespace() {
                self.space = true;
                continue;
            }
            self.settle();
            self.out.push(ch);
        }
    }

    /// A literal the extractor is emitting itself — `- `, `## `, `[`. Pending whitespace is
    /// honoured, so `word [link]` keeps its space.
    fn marker(&mut self, text: &str) {
        self.settle();
        self.out.push_str(text);
    }

    /// A literal that must sit flush against what came before — `](target)`. Pending whitespace
    /// is dropped, or `<a>text </a>` would render as `text ](url)`.
    fn tight_marker(&mut self, text: &str) {
        self.space = false;
        self.settle();
        self.out.push_str(text);
    }

    fn line_break(&mut self, count: usize) {
        self.breaks = self.breaks.max(count);
    }

    fn settle(&mut self) {
        if self.breaks > 0 {
            if !self.out.is_empty() {
                while self.out.ends_with(' ') {
                    self.out.pop();
                }
                let existing =
                    self.out.chars().rev().take_while(|ch| *ch == '\n').count();
                for _ in existing..self.breaks {
                    self.out.push('\n');
                }
            }
            self.breaks = 0;
        } else if self.space && !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push(' ');
        }
        self.space = false;
    }

    fn finish(self) -> String {
        self.out.trim().to_owned()
    }
}

// ---------------------------------------------------------------------------------------
// Search result parsing
// ---------------------------------------------------------------------------------------

/// Reads results out of a DuckDuckGo-shaped HTML page.
///
/// The shape, measured against the live endpoint on 2026-08-13: each result is an
/// `<a class="result__a" href="…">title</a>` followed by an
/// `<a class="result__snippet" href="…">blurb</a>`, and **the `href` is a redirect wrapper**
/// — `//duckduckgo.com/l/?uddg=<percent-encoded real url>&rut=…`. The `uddg` parameter is
/// unwrapped here, because handing an agent a tracking redirect instead of the page's own
/// address makes every result look like it is hosted by the search engine.
///
/// Every `href` is **entity-decoded before it is used as a URL**, because an attribute value
/// carries `&` as `&amp;` and a URL handed to an agent with `&amp;` still in its query is a
/// different URL from the one on the page. That bites on a *direct* href — a result whose
/// address is not wrapped — and `a_direct_result_url_has_its_entities_decoded` is the test that
/// fails without it. It happens **not** to bite on the wrapper, since splitting on `&` finds
/// the `&` of `&amp;` either way; the earlier version of this comment claimed otherwise, and
/// the A/B that was supposed to prove it passed on the broken build, which is how the claim
/// was caught.
pub fn parse_html_results(html: &str) -> Vec<SearchResult> {
    let bytes = html.as_bytes();
    let mut results: Vec<SearchResult> = Vec::new();
    let mut i = 0usize;

    while let Some(at) = find_ci(html, "<a", i) {
        let Some(end) = tag_end(bytes, at) else { break };
        let source = html.get(at + 1..end).unwrap_or("");
        i = end + 1;
        if tag_name(source) != "a" {
            continue;
        }
        let class = attribute(source, "class").unwrap_or_default();
        let is_title = class.split_whitespace().any(|token| token == "result__a");
        let is_snippet = class.split_whitespace().any(|token| token == "result__snippet");
        if !is_title && !is_snippet {
            continue;
        }

        let (inner, next) = raw_until_close(html, bytes, i, "a");
        i = next;
        let text = html_to_text(&inner, None).text;

        if is_title {
            let Some(href) = attribute(source, "href") else { continue };
            let Some(url) = unwrap_redirect(&href) else { continue };
            results.push(SearchResult { title: text, url, snippet: String::new() });
        } else if let Some(last) = results.last_mut()
            && last.snippet.is_empty()
        {
            last.snippet = text;
        }
    }
    results
}

/// Unwraps DuckDuckGo's `/l/?uddg=` redirect, and normalises a protocol-relative href.
///
/// Returns `None` for anything that is not an http(s) URL once unwrapped, so an internal
/// DuckDuckGo link never reaches an agent dressed as a result.
fn unwrap_redirect(href: &str) -> Option<String> {
    let href = decode_entities(href.trim());
    // `starts_with` rather than `strip_prefix`: the stripped form borrows `href`, and the
    // other arm has to *move* it. Testing a `bool` keeps the two arms independent.
    let absolute = if href.starts_with("//") { format!("https:{href}") } else { href };

    if let Some(at) = absolute.find("uddg=") {
        let tail = absolute.get(at + 5..).unwrap_or("");
        let value = tail.split('&').next().unwrap_or("");
        let decoded = percent_decode(value);
        return split_url(&decoded).map(|_| decoded);
    }
    split_url(&absolute).map(|_| absolute)
}

/// Reads results out of a SearXNG-shaped JSON document: `{"results":[{title,url,content}]}`.
///
/// Tolerant about the field carrying the blurb — SearXNG calls it `content` and several
/// forks call it `snippet` — because a result with no blurb is still a result and refusing the
/// whole document over a field name would be a silent empty list by another route.
pub fn parse_searxng_results(json: &str) -> Vec<SearchResult> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(array) = value.get("results").and_then(|results| results.as_array()) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|entry| {
            let url = entry.get("url").and_then(|url| url.as_str())?;
            split_url(url)?;
            let title = entry
                .get("title")
                .and_then(|title| title.as_str())
                .unwrap_or(url)
                .trim()
                .to_owned();
            let snippet = ["content", "snippet", "description"]
                .iter()
                .find_map(|field| entry.get(*field).and_then(|found| found.as_str()))
                .unwrap_or("")
                .trim()
                .to_owned();
            Some(SearchResult { title, url: url.to_owned(), snippet })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------------------

/// The mutable half, behind one lock.
#[derive(Default)]
struct State {
    limiter: RateLimiter,
    cookies: CookieJar,
    /// Per host, with the moment it was read, so [`ROBOTS_TTL`] can expire it.
    robots: HashMap<String, (Robots, Instant)>,
}

/// A browsing session: one connection pool, one cookie jar, one pacing table, one robots cache.
///
/// Held for as long as the MCP server runs, which is what makes the cookie jar and the
/// connection reuse worth anything — both are session-shaped, and a fresh client per request
/// would have neither.
pub struct Research {
    agent: ureq::Agent,
    config: ResearchConfig,
    state: Mutex<State>,
}

impl Default for Research {
    fn default() -> Self {
        Self::new(ResearchConfig::default())
    }
}

impl Research {
    pub fn new(config: ResearchConfig) -> Self {
        // Three settings here are load-bearing and each is a decision:
        //
        // `http_status_as_error(false)` — `ureq` turns a 4xx/5xx into `Err` by default, which
        // would throw away the status code this module exists to report. A 403 must arrive as
        // a response we can name, not as a transport failure.
        //
        // `max_redirects(0)` — redirects are followed by hand in `fetch_at`, so that
        // `robots.txt` and the pacing table are consulted for *every* hop. A redirect to
        // another host is a request to another host, and `ureq` following it internally would
        // make that request without either check.
        //
        // `timeout_global` — one ceiling on the whole exchange, so a server that accepts a
        // connection and then says nothing cannot hold a tool call open.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .user_agent(config.user_agent.clone())
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        Self { agent, config, state: Mutex::new(State::default()) }
    }

    pub fn config(&self) -> &ResearchConfig {
        &self.config
    }

    /// Fetches a page and extracts its readable text.
    ///
    /// One of this module's two clock reads. See the module doc comment.
    pub fn fetch(&self, url: &str) -> Result<Page, ResearchError> {
        self.fetch_at(url, Instant::now())
    }

    /// [`Research::fetch`] with the moment supplied.
    ///
    /// # The redirect loop is where the policy is actually enforced
    ///
    /// Every hop re-derives the host, re-checks `robots.txt` for the *new* URL and re-paces
    /// against the *new* host. A chain that starts on a permitted host and ends on a
    /// disallowed one is refused at the hop that reaches it, which is the only place the
    /// refusal is true.
    ///
    /// `now` advances by exactly the time this function chooses to wait. It never reads the
    /// clock again, so the pacing table is fed the *scheduled* time rather than the wall clock
    /// — and since a real request takes time that this does not count, the effect is to pace
    /// slightly more conservatively than required. Erring in that direction is the point.
    pub fn fetch_at(&self, url: &str, now: Instant) -> Result<Page, ResearchError> {
        let mut target = url.to_owned();
        let mut now = now;

        for _ in 0..MAX_REDIRECTS {
            let parts = split_url(&target).ok_or_else(|| ResearchError::NotHttp(target.clone()))?;
            // ⚠ **Before the robots check, not after it.** `fetch_robots` makes a real request
            // to this host, so a host policy consulted afterwards has already let a request
            // out to the address it exists to refuse — and worse, an unreachable `robots.txt`
            // degrades to `Robots::permissive`, so a local address was *explicitly allowed*.
            self.check_host(&target, &parts)?;
            self.check_robots(&parts, &mut now)?;
            now = self.pace(&parts.host, now)?;

            let (status, headers_location, body, truncated) = self.request(&target, &parts)?;

            if (300..400).contains(&status) {
                let Some(location) = headers_location else {
                    return Err(ResearchError::Refused { url: target, status });
                };
                let Some(next) = resolve_url(&target, &location) else {
                    return Err(ResearchError::Refused { url: target, status });
                };
                target = next;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(ResearchError::Refused { url: target, status });
            }

            let html = String::from_utf8_lossy(&body);
            let readable = html_to_text(&html, Some(&target));
            let (text, clipped) = clip(readable.text, self.config.max_text_chars);
            return Ok(Page {
                url: target,
                status,
                title: readable.title,
                text,
                truncated: truncated || clipped,
            });
        }
        Err(ResearchError::TooManyRedirects { url: url.to_string() })
    }

    /// Searches, through whichever engine is configured.
    ///
    /// The other clock read. See the module doc comment.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, ResearchError> {
        self.search_at(query, limit, Instant::now())
    }

    /// [`Research::search`] with the moment supplied.
    ///
    /// **The search request goes through the same robots check and the same pacing table as
    /// any other fetch.** It costs nothing and it means the policy has no exception carved
    /// into it — which is the difference between a policy and a preference.
    ///
    /// ⚠ **The one thing it does not go through is `Research::check_host`, and that is the
    /// distinction the host policy is actually drawn along: who chose the URL.** The search
    /// endpoint is *user configuration* — [`SEARCH_ENDPOINT_ENV`], set by hand — and a SearXNG
    /// on `localhost` is the ordinary way to run one. A URL a model produced is a different
    /// thing entirely and is checked. Note that the results a search returns are only ever
    /// followed through [`Research::fetch_at`], which does check, so an endpoint that answered
    /// with a list of local addresses still cannot get one fetched.
    pub fn search_at(
        &self,
        query: &str,
        limit: usize,
        now: Instant,
    ) -> Result<Vec<SearchResult>, ResearchError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(ResearchError::SearchUnreadable {
                endpoint: self.config.search.endpoint().unwrap_or("(none)").to_owned(),
                message: "the query was empty".to_owned(),
            });
        }

        let (endpoint, json) = match &self.config.search {
            SearchEngine::Off => {
                return Err(ResearchError::NoSearchEngine(format!(
                    "web search is switched off ({SEARCH_ENDPOINT_ENV} is set to `off`). \
                     Point {SEARCH_ENDPOINT_ENV} at a search endpoint to turn it back on; \
                     research_fetch still works on any URL you already have."
                )));
            }
            SearchEngine::Html { endpoint } => (endpoint.clone(), false),
            SearchEngine::Json { endpoint } => (endpoint.clone(), true),
        };

        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let mut url = format!("{endpoint}{separator}q={}", percent_encode_query(query));
        if json {
            url.push_str("&format=json");
        }

        let parts = split_url(&url).ok_or_else(|| ResearchError::NotHttp(url.clone()))?;
        let mut clock = now;
        self.check_robots(&parts, &mut clock)?;
        self.pace(&parts.host, clock)?;

        let (status, _, body, _) = self.request(&url, &parts)?;
        if !(200..300).contains(&status) {
            // A 202 from DuckDuckGo's anomaly page and a 429 from a paced SearXNG both land
            // here, and both are reported with their status rather than retried in disguise.
            return Err(ResearchError::Refused { url, status });
        }

        let text = String::from_utf8_lossy(&body);
        let mut results =
            if json { parse_searxng_results(&text) } else { parse_html_results(&text) };
        if results.is_empty() {
            return Err(ResearchError::SearchUnreadable {
                endpoint,
                message: "the endpoint answered 200 and no results could be read out of it — \
                          either the query genuinely matched nothing, or the answer's shape \
                          has changed"
                    .to_owned(),
            });
        }
        results.truncate(limit.clamp(1, 25));
        Ok(results)
    }

    /// One request, with the cookie jar on both sides of it.
    ///
    /// Returns the status, the `Location` header, the capped body, and whether the cap was hit.
    #[allow(clippy::type_complexity)]
    fn request(
        &self,
        url: &str,
        parts: &UrlParts,
    ) -> Result<(u16, Option<String>, Vec<u8>, bool), ResearchError> {
        let cookie = {
            let state = self.lock();
            state.cookies.header_for(&parts.host, &parts.path, parts.is_https())
        };

        let mut request = self
            .agent
            .get(url)
            .header("Accept", ACCEPT)
            .header("Accept-Language", ACCEPT_LANGUAGE);
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }

        let response = request.call().map_err(|error| ResearchError::Network {
            url: url.to_owned(),
            message: error.to_string(),
        })?;

        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let set_cookies: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();

        {
            let mut state = self.lock();
            for header in &set_cookies {
                state.cookies.store(header, &parts.host, &parts.path);
            }
        }

        // `take(cap + 1)` rather than `take(cap)`: reading one byte past the ceiling is what
        // makes "was this truncated" a fact rather than a guess about a body that happened to
        // be exactly the cap long.
        let cap = self.config.max_bytes;
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(cap as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|error| ResearchError::Body {
                url: url.to_owned(),
                message: error.to_string(),
            })?;
        let truncated = body.len() > cap;
        body.truncate(cap);

        Ok((status, location, body, truncated))
    }

    /// Applies the host policy to one URL. See the module header for what it refuses and why.
    ///
    /// Two checks, and both are needed. The **name** answers without a lookup and catches
    /// every literal address and the four suffixes that can only mean a local machine. The
    /// **resolved addresses** catch the case the name cannot: a perfectly ordinary public
    /// hostname whose A record is `127.0.0.1`, which is how this is done deliberately.
    ///
    /// Every address is checked, not the first: a name that resolves to a public address and a
    /// private one is a name that reaches the private one whenever the resolver feels like it.
    ///
    /// ⚠ **Known and not fixed: the lookup is blocking and outside `config.timeout`.**
    /// `to_socket_addrs` is the platform resolver and takes no deadline, so a host whose DNS
    /// hangs blocks this call for however long the system resolver waits — per redirect hop,
    /// since the policy is applied on each. `timeout` bounds the HTTP exchange below and has
    /// never bounded this. Giving it one means resolving on a worker with a deadline, which is
    /// a change to how every fetch is made rather than a line here.
    fn check_host(&self, url: &str, parts: &UrlParts) -> Result<(), ResearchError> {
        if self.config.allow_local_hosts {
            return Ok(());
        }
        let hostname = hostname_of(&parts.host);
        let refuse = |reason: &str| ResearchError::BlockedHost {
            url: url.to_owned(),
            host: hostname.to_owned(),
            reason: reason.to_owned(),
        };
        if let Some(reason) = blocked_by_name(hostname) {
            return Err(refuse(reason));
        }

        // The port is irrelevant to what a name resolves to, so a fixed one keeps this to one
        // lookup shape. A resolution failure is a *network* error and not a policy refusal —
        // it is the one thing here that is genuinely worth retrying.
        let resolved = (hostname, 80u16).to_socket_addrs().map_err(|error| {
            ResearchError::Network { url: url.to_owned(), message: error.to_string() }
        })?;
        for address in resolved {
            if let Some(reason) = blocked_ip(address.ip()) {
                return Err(refuse(reason));
            }
        }
        Ok(())
    }

    /// Consults, and if necessary fetches, this host's `robots.txt`.
    fn check_robots(&self, parts: &UrlParts, now: &mut Instant) -> Result<(), ResearchError> {
        if !self.config.respect_robots {
            return Ok(());
        }
        if is_robots_path(&parts.path) {
            return Ok(()); // Fetching the rules is never itself subject to them.
        }

        let cached = {
            let state = self.lock();
            state.robots.get(&parts.host).and_then(|(robots, read)| {
                (now.saturating_duration_since(*read) < ROBOTS_TTL).then(|| robots.clone())
            })
        };

        let robots = match cached {
            Some(robots) => robots,
            None => {
                let fetched = self.fetch_robots(parts, now);
                if let Some(delay) = fetched.crawl_delay() {
                    self.lock().limiter.set_interval(&parts.host, delay);
                }
                self.lock().robots.insert(parts.host.clone(), (fetched.clone(), *now));
                fetched
            }
        };

        robots.allows(&parts.path).map_err(|rule| ResearchError::RobotsDisallowed {
            url: format!("{}{}", parts.origin(), parts.path),
            host: parts.host.clone(),
            rule,
        })
    }

    /// Reads and parses `/robots.txt`, degrading to [`Robots::permissive`] on anything but a
    /// 2xx with a body. See that function for why an unreachable file permits rather than
    /// refuses.
    ///
    /// ⚠ **The first fetch to any host therefore costs about [`DEFAULT_MIN_INTERVAL`] extra**,
    /// because this is a request to that host and the page behind it is a second one: the
    /// pacing table quite correctly makes the second wait out the interval. That is by design
    /// and not a stall — but it is a second and a half that appears from nowhere if you do not
    /// know it is there, so it is written down rather than left to be profiled. Later fetches
    /// to the same host skip it entirely, since the parsed rules are cached for
    /// [`ROBOTS_TTL`].
    fn fetch_robots(&self, parts: &UrlParts, now: &mut Instant) -> Robots {
        let url = format!("{}/robots.txt", parts.origin());
        let robots_parts = UrlParts {
            scheme: parts.scheme.clone(),
            host: parts.host.clone(),
            path: "/robots.txt".to_owned(),
        };
        // Paced like any other request. A failure to pace is not a reason to refuse the page —
        // the caller's own pacing check will catch a host that is genuinely too busy.
        if let Ok(advanced) = self.pace(&parts.host, *now) {
            *now = advanced;
        }
        match self.request(&url, &robots_parts) {
            Ok((status, _, body, _)) if (200..300).contains(&status) => {
                Robots::parse(&String::from_utf8_lossy(&body), ROBOTS_TOKEN)
            }
            _ => Robots::permissive(),
        }
    }

    /// Waits out this host's interval, or refuses when the wait is longer than a tool call
    /// should be. Returns the moment the request is being made at.
    fn pace(&self, host: &str, now: Instant) -> Result<Instant, ResearchError> {
        let delay = {
            let state = self.lock();
            state.limiter.delay_before(host, self.config.min_interval, now)
        };
        if delay > MAX_PACING_WAIT {
            return Err(ResearchError::RateLimited {
                host: host.to_owned(),
                seconds: delay.as_secs().max(1),
            });
        }
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let at = now.checked_add(delay).unwrap_or(now);
        self.lock().limiter.note(host, at);
        Ok(at)
    }

    /// The lock, recovered rather than propagated on poison.
    ///
    /// A panic in another thread while it held this must not make every later research call
    /// fail — the state behind it is a cache and a pacing table, and neither is corrupted by
    /// being abandoned halfway.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Cuts a string to a character count, on a character boundary, saying so when it cut.
fn clip(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n\n[truncated by Velm: the page is longer than this tool returns]");
    (out, true)
}

// ---------------------------------------------------------------------------------------
// Tests — every one of them offline, against a string literal
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod robots_tests {
    use super::*;

    /// The precedence rule sites are actually written against: longest pattern wins, and a tie
    /// goes to `Allow`. Without the tie rule, `Allow: /w/api.php` under `Disallow: /w/api.php`
    /// — how a site carves one endpoint out of a blanket refusal — would silently refuse.
    #[test]
    fn the_longest_matching_rule_wins_and_a_tie_goes_to_allow() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /w/\nAllow: /w/api.php\nDisallow: /w/api.php\n",
            ROBOTS_TOKEN,
        );
        assert!(robots.allows("/w/api.php").is_ok(), "a tie must go to Allow");
        assert!(robots.allows("/w/load.php").is_err(), "the blanket rule must still bite");
        assert!(robots.allows("/wiki/Rust").is_ok(), "an unmatched path is allowed");
    }

    /// A group naming us beats the wildcard group outright — including when it is *empty*,
    /// which is how a site says "Velm may read everything" while restricting others.
    #[test]
    fn a_group_naming_velm_wins_even_when_it_is_empty() {
        let text = "User-agent: *\nDisallow: /\n\nUser-agent: Velm\nDisallow:\n";
        let robots = Robots::parse(text, ROBOTS_TOKEN);
        assert!(robots.allows("/anything").is_ok());

        // And the wildcard group is what applies when there is no group for us.
        let others = Robots::parse("User-agent: *\nDisallow: /\n", ROBOTS_TOKEN);
        assert!(others.allows("/anything").is_err());
    }

    /// An empty `Disallow:` is the spelling of "no restriction". Storing it as a rule matching
    /// the empty prefix would match every path there is and lock the entire site.
    #[test]
    fn an_empty_disallow_permits_rather_than_forbids_everything() {
        let robots = Robots::parse("User-agent: *\nDisallow:\n", ROBOTS_TOKEN);
        assert!(robots.allows("/").is_ok());
        assert!(robots.allows("/deep/path?x=1").is_ok());
    }

    /// The two wildcards, and the query string. `Disallow: /*?` is how a site refuses its own
    /// search result pages, and dropping the query before matching would quietly allow exactly
    /// those.
    #[test]
    fn patterns_understand_a_star_a_dollar_and_a_query_string() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /*?\nDisallow: /private/\nDisallow: /*.pdf$\n",
            ROBOTS_TOKEN,
        );
        assert!(robots.allows("/page").is_ok());
        assert!(robots.allows("/page?q=1").is_err(), "a query string was not matched");
        assert!(robots.allows("/private/x").is_err());
        assert!(robots.allows("/docs/manual.pdf").is_err());
        assert!(robots.allows("/docs/manual.pdf.html").is_ok(), "$ must anchor the end");
    }

    /// A malformed file must never be able to fail a fetch. This one has a rule before any
    /// group, a directive with no colon, a comment mid-line, an unknown field and a crawl delay
    /// that is not a number.
    #[test]
    fn a_malformed_robots_file_is_ignored_rather_than_fatal() {
        let text = "Disallow: /orphan\n\
                    this is not a directive\n\
                    User-agent: *   # everyone\n\
                    Sitemap: https://example.com/sitemap.xml\n\
                    Crawl-delay: soon\n\
                    Disallow: /admin\n\
                    \u{4e2d}\u{6587}: \u{4e2d}\u{6587}\n";
        let robots = Robots::parse(text, ROBOTS_TOKEN);
        assert!(robots.allows("/admin").is_err());
        assert!(robots.allows("/orphan").is_ok(), "a rule before any group belongs to nobody");
        assert_eq!(robots.crawl_delay(), None);

        // And an empty file, and one that is only whitespace.
        assert!(Robots::parse("", ROBOTS_TOKEN).allows("/").is_ok());
        assert!(Robots::parse("\n\n   \n", ROBOTS_TOKEN).allows("/").is_ok());
    }

    /// A published `Crawl-delay` is read, and clamped — a site asking for a day is not going to
    /// be waited out inside a tool call.
    #[test]
    fn a_crawl_delay_is_read_and_clamped() {
        let robots = Robots::parse("User-agent: *\nCrawl-delay: 2.5\n", ROBOTS_TOKEN);
        assert_eq!(robots.crawl_delay(), Some(Duration::from_millis(2500)));

        let absurd = Robots::parse("User-agent: *\nCrawl-delay: 86400\n", ROBOTS_TOKEN);
        assert_eq!(absurd.crawl_delay(), Some(Duration::from_secs(60)));
    }

    /// Consecutive `User-agent:` lines share one group; one after a rule starts a new one.
    /// Getting this wrong applies another agent's restrictions to us, silently.
    #[test]
    fn consecutive_agent_lines_share_a_group() {
        let text = "User-agent: velm\nUser-agent: someoneelse\nDisallow: /shared\n\
                    User-agent: someoneelse\nDisallow: /theirs\n";
        let robots = Robots::parse(text, ROBOTS_TOKEN);
        assert!(robots.allows("/shared").is_err(), "the shared group applies to us");
        assert!(robots.allows("/theirs").is_ok(), "another agent's own group does not");
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    /// The whole point of taking `now` as a parameter: the interval is arithmetic, not a wait.
    #[test]
    fn a_host_is_paced_by_arithmetic_rather_than_by_waiting() {
        let base = Instant::now();
        let floor = Duration::from_millis(1500);
        let mut limiter = RateLimiter::new();

        assert_eq!(limiter.delay_before("example.com", floor, base), Duration::ZERO);
        limiter.note("example.com", base);

        let straight_after = limiter.delay_before("example.com", floor, base);
        assert_eq!(straight_after, floor, "a second request must wait the whole interval");

        let part_way = base + Duration::from_millis(1000);
        assert_eq!(limiter.delay_before("example.com", floor, part_way), Duration::from_millis(500));

        let later = base + Duration::from_millis(1600);
        assert_eq!(limiter.delay_before("example.com", floor, later), Duration::ZERO);

        // Another host is not paced by this one's traffic.
        assert_eq!(limiter.delay_before("other.example", floor, base), Duration::ZERO);
    }

    /// A published `Crawl-delay` may slow us down and may never speed us up. A site asking for
    /// 100ms does not get 100ms; it gets the floor.
    #[test]
    fn a_crawl_delay_raises_the_interval_and_never_lowers_it() {
        let base = Instant::now();
        let floor = Duration::from_millis(1500);
        let mut limiter = RateLimiter::new();

        limiter.set_interval("slow.example", Duration::from_secs(5));
        limiter.note("slow.example", base);
        assert_eq!(
            limiter.delay_before("slow.example", floor, base + Duration::from_secs(2)),
            Duration::from_secs(3)
        );

        limiter.set_interval("fast.example", Duration::from_millis(10));
        limiter.note("fast.example", base);
        assert_eq!(
            limiter.delay_before("fast.example", floor, base),
            floor,
            "a site cannot ask to be polled faster than the floor"
        );
    }
}

#[cfg(test)]
mod cookie_tests {
    use super::*;

    /// A session cookie set on one request comes back on the next, which is the entire reason
    /// the jar exists — a consent redirect that sets a token and bounces back is otherwise an
    /// infinite loop.
    #[test]
    fn a_cookie_set_on_one_request_returns_on_the_next() {
        let mut jar = CookieJar::new();
        jar.store("session=abc123; Path=/; HttpOnly", "example.com", "/articles/1");
        assert_eq!(
            jar.header_for("example.com", "/articles/2", true).as_deref(),
            Some("session=abc123")
        );
    }

    /// The two refusals that matter: a cookie cannot be set for a domain we are not on, and a
    /// `Secure` cookie is never sent in clear.
    #[test]
    fn a_cookie_cannot_widen_past_its_host_or_leak_over_http() {
        let mut jar = CookieJar::new();
        jar.store("tracker=1; Domain=.com", "example.com", "/");
        // Refused as a widening; kept as host-only, so a sibling registrable domain sees nothing.
        assert_eq!(jar.header_for("elsewhere.com", "/", true), None);
        assert!(jar.header_for("example.com", "/", true).is_some());

        jar.store("evil=1; Domain=other.example", "example.com", "/");
        assert_eq!(jar.header_for("other.example", "/", true), None, "a cross-site set was kept");

        let mut secure = CookieJar::new();
        secure.store("token=t; Secure", "example.com", "/");
        assert_eq!(secure.header_for("example.com", "/", false), None, "Secure leaked over http");
        assert!(secure.header_for("example.com", "/", true).is_some());
    }

    /// A legitimate widening to the registrable domain still works, or every site that serves
    /// `www.` and sets a cookie on the bare domain loses its session on the first redirect.
    #[test]
    fn a_cookie_widens_to_its_own_parent_domain() {
        let mut jar = CookieJar::new();
        jar.store("id=7; Domain=example.com; Path=/", "www.example.com", "/");
        assert!(jar.header_for("shop.example.com", "/", true).is_some());
        assert!(jar.header_for("example.com", "/", true).is_some());
    }

    /// Path scoping, including the boundary case: `/foo` must not match `/foobar`.
    #[test]
    fn a_path_scoped_cookie_stays_in_its_path() {
        let mut jar = CookieJar::new();
        jar.store("deep=1; Path=/foo", "example.com", "/");
        assert!(jar.header_for("example.com", "/foo", true).is_some());
        assert!(jar.header_for("example.com", "/foo/bar", true).is_some());
        assert_eq!(jar.header_for("example.com", "/foobar", true), None);
        assert_eq!(jar.header_for("example.com", "/other", true), None);
    }

    /// Re-setting a name replaces it. Appending would make a rotating session token grow a
    /// history and send every value it ever had.
    #[test]
    fn resetting_a_cookie_replaces_it_and_the_jar_is_bounded() {
        let mut jar = CookieJar::new();
        jar.store("s=1", "example.com", "/");
        jar.store("s=2", "example.com", "/");
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.header_for("example.com", "/", true).as_deref(), Some("s=2"));

        for index in 0..200 {
            jar.store(&format!("c{index}=x"), "example.com", "/");
        }
        assert!(jar.len() <= CookieJar::PER_DOMAIN + 1, "the per-domain cap did not hold");
    }
}

#[cfg(test)]
mod entity_tests {
    use super::*;

    /// The bug this repo shipped twice. A doubly-escaped entity must decode **one** level:
    /// `&amp;#38;` is the text `&#38;`, not the character `&`. A single left-to-right pass
    /// cannot get this wrong, because the `&` it writes is never re-examined.
    #[test]
    fn a_doubly_escaped_entity_decodes_exactly_one_level() {
        assert_eq!(decode_entities("&amp;#38;"), "&#38;");
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("&amp;amp;"), "&amp;");
        // And the ordinary cases still work.
        assert_eq!(decode_entities("&amp;"), "&");
        assert_eq!(decode_entities("&#38;"), "&");
        assert_eq!(decode_entities("Conversions &amp; Swaps"), "Conversions & Swaps");
    }

    #[test]
    fn numeric_named_and_hex_entities_all_decode() {
        assert_eq!(decode_entities("wouldn&#39;t"), "wouldn't");
        assert_eq!(decode_entities("&#x27;"), "'");
        assert_eq!(decode_entities("&#X2014;"), "—");
        assert_eq!(decode_entities("a &lt;b&gt; c"), "a <b> c");
        assert_eq!(decode_entities("&hellip;"), "…");
        assert_eq!(decode_entities("&#128512;"), "\u{1f600}");
    }

    /// An `&` that begins nothing is a literal ampersand. A page saying "Tom & Jerry" is common
    /// and is not an error, and a decoder that swallowed it would eat the word after it.
    #[test]
    fn an_unrecognised_ampersand_survives_as_itself() {
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("a&b"), "a&b");
        assert_eq!(decode_entities("&notanentity;"), "&notanentity;");
        assert_eq!(decode_entities("&"), "&");
        assert_eq!(decode_entities("&#;"), "&#;");
        assert_eq!(decode_entities("&#99999999;"), "&#99999999;", "an unassigned code point");
    }

    /// `strip_site_affix` aborted this process twice on a byte index that landed inside a
    /// character, and `panic = "abort"` means there is no catching it. The window this scanner
    /// bounds its search with is 12 bytes — exactly the shape that bug had.
    #[test]
    fn multibyte_text_around_an_ampersand_never_panics() {
        for text in [
            "&\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}",
            "\u{4e2d}\u{6587}&amp;\u{4e2d}\u{6587}",
            "&\u{1f600}\u{1f600}\u{1f600}\u{1f600};",
            "caf\u{e9} &amp; cr\u{e8}me",
            &"\u{4e2d}&".repeat(500),
        ] {
            let decoded = decode_entities(text);
            assert!(!decoded.is_empty() || text.is_empty());
        }
        assert_eq!(decode_entities("\u{4e2d}\u{6587}&amp;\u{4e2d}\u{6587}"), "中文&中文");
    }
}

#[cfg(test)]
mod extraction_tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html>
<html><head>
<title>  Torque   specifications  </title>
<style>body { color: red; }</style>
<script>var x = "<p>not text</p>";</script>
</head>
<body>
<nav><a href="/menu">Menu</a><a href="/about">About</a></nav>
<h1>Torque specifications</h1>
<p>The figure is 45&nbsp;Nm, per the <a href="/manual/ch3.html">chapter&nbsp;3</a> table.</p>
<ul><li>Wheel: 120 Nm</li><li>Sump: 25 Nm</li></ul>
<p>See also <a href="https://example.org/other">another site</a>.</p>
<img src="/x.png" alt="a > b">
<footer>Copyright &copy; nobody</footer>
</body></html>"#;

    /// The whole extractor on one page: the title, the stripped elements, the headings, the
    /// bullets, the entity decoding, and both link forms.
    #[test]
    fn a_page_becomes_readable_text_with_its_structure_kept() {
        let readable = html_to_text(PAGE, Some("https://example.com/specs/index.html"));
        assert_eq!(readable.title.as_deref(), Some("Torque specifications"));

        let text = &readable.text;
        assert!(text.contains("# Torque specifications"), "{text}");
        assert!(text.contains("- Wheel: 120 Nm"), "{text}");
        // `&nbsp;` decodes to U+00A0, which `char::is_whitespace` answers true for — so the
        // collapser folds it into an ordinary space, which is what a reader wants and what a
        // model tokenises sensibly. Asserted, because the alternative (a non-breaking space
        // surviving into extracted text) is invisible in a diff and confusing in a prompt.
        assert!(text.contains("45 Nm"), "{text}");
        assert!(!text.contains('\u{a0}'), "a non-breaking space survived into the text");

        // A relative link is resolved against the page; an absolute one is left alone.
        assert!(text.contains("[chapter 3](https://example.com/manual/ch3.html)"), "{text}");
        assert!(text.contains("[another site](https://example.org/other)"), "{text}");

        // Stripped whole: script bodies, style rules, the nav and the footer.
        assert!(!text.contains("not text"), "a script body leaked: {text}");
        assert!(!text.contains("color: red"), "a stylesheet leaked: {text}");
        assert!(!text.contains("Menu"), "the nav leaked: {text}");
        assert!(!text.contains("Copyright"), "the footer leaked: {text}");

        // And no markup survived. The `>` inside `alt="a > b"` is the case that would cut a tag
        // in half and spill the rest of it into the text.
        assert!(!text.contains('<'), "markup leaked: {text}");
    }

    /// A site that marks its chrome with ARIA landmarks rather than with `<nav>`/`<footer>`
    /// gets it stripped anyway. A/B'd against a real 240 KB Wikipedia capture: 43,074
    /// characters by tag name alone, 42,535 with roles as well — the search box, *Personal
    /// tools*, *24 languages* and the navbox `v · t · e` links, and no article text.
    ///
    /// The corresponding refusal matters as much: `role="main"` and `role="article"` are *not*
    /// chrome, and a rule that swept up any `role=` would throw away the article on precisely
    /// the sites careful enough to label it.
    #[test]
    fn aria_landmarks_are_stripped_and_content_roles_are_not() {
        let page = r#"<div role="navigation"><a href="/x">Menu</a></div>
                      <div ROLE="Banner">Site name</div>
                      <span aria-hidden="true">decorative</span>
                      <div data-role="navigation">not a landmark</div>
                      <main role="main"><p>The article itself.</p></main>
                      <div role="contentinfo">Copyright</div>"#;
        let text = html_to_text(page, None).text;
        assert!(text.contains("The article itself."), "{text}");
        assert!(text.contains("not a landmark"), "a `data-role` was read as a role: {text}");
        for chrome in ["Menu", "Site name", "decorative", "Copyright"] {
            assert!(!text.contains(chrome), "`{chrome}` survived the landmark strip: {text}");
        }
    }

    /// ⚠ The one that cost a real article. A **void** element carrying a landmark attribute
    /// has no closing tag, so skipping it as a container discards everything after it.
    /// Wikipedia's logo is exactly this — `<img … aria-hidden="true">` — and against a real
    /// capture it swallowed 226,863 bytes and left 60 characters of a 42,884-character page.
    ///
    /// The assertion is on the text **after** the void element, because the failure is silent
    /// in every other respect: the extraction still returns, still has a title, and still
    /// contains everything before the image.
    #[test]
    fn a_void_element_is_never_skipped_as_a_container() {
        let page = r#"<p>Before.</p>
                      <img src="/logo.svg" alt="" aria-hidden="true" width="50">
                      <p>The whole rest of the article.</p>
                      <input type="text" role="search">
                      <p>And this too.</p>
                      <br aria-hidden="true">
                      <p>And this.</p>"#;
        let text = html_to_text(page, None).text;
        assert!(text.contains("Before."), "{text}");
        assert!(text.contains("The whole rest of the article."), "an <img> ate the page: {text}");
        assert!(text.contains("And this too."), "an <input> ate the page: {text}");
        assert!(text.contains("And this."), "a <br> ate the page: {text}");
    }

    /// Blocks separate; runs of closing tags do not each produce a blank line.
    #[test]
    fn whitespace_is_collapsed_and_blocks_are_separated_once() {
        let readable = html_to_text("<p>one</p></div></section><p>two</p>", None);
        assert_eq!(readable.text, "one\n\ntwo");

        let spaced = html_to_text("<p>  a   \n\n  b  </p>", None);
        assert_eq!(spaced.text, "a b");

        let empty = html_to_text("<html><body></body></html>", None);
        assert_eq!(empty.text, "");
        assert_eq!(empty.title, None);
    }

    /// Malformed input must produce text rather than a panic: an unclosed tag, an unclosed
    /// anchor, a `<` that begins nothing, a comment that never ends, and CJK either side of
    /// every one of them.
    #[test]
    fn malformed_markup_and_multibyte_text_never_panic() {
        for html in [
            "<p>\u{4e2d}\u{6587}",
            "<a href=\"/x\">\u{4e2d}\u{6587}",
            "\u{4e2d}<",
            "<!-- \u{4e2d}\u{6587}",
            "<p attr=\"\u{4e2d}>\u{6587}\">text</p>",
            "<<<>>>\u{4e2d}",
            "<script>\u{4e2d}",
            "<h9>\u{4e2d}</h9>",
        ] {
            let readable = html_to_text(html, Some("https://example.com/a/b"));
            assert!(!readable.text.contains('\u{0}'));
        }
        // The unclosed anchor still closes its own bracket rather than leaving a dangling `[`.
        let dangling = html_to_text("<a href=\"/x\">text", Some("https://example.com/"));
        assert!(dangling.text.ends_with("](https://example.com/x)"), "{}", dangling.text);
    }

    /// A `mailto:` or `javascript:` href is not a relative path and must not be resolved into
    /// one — `https://example.com/mailto:a@b` is a link to nowhere.
    #[test]
    fn a_non_http_href_is_left_out_rather_than_resolved() {
        let readable = html_to_text(
            r#"<a href="mailto:a@b.com">mail</a> <a href="javascript:void(0)">js</a>"#,
            Some("https://example.com/page"),
        );
        assert!(!readable.text.contains("example.com/mailto"), "{}", readable.text);
        assert!(!readable.text.contains("]("), "{}", readable.text);
        assert!(readable.text.contains("mail"), "the link's words are still text");
    }

    #[test]
    fn relative_urls_resolve_the_four_forms_that_occur() {
        let base = "https://example.com/docs/guide/page.html?v=2";
        assert_eq!(
            resolve_url(base, "other.html").as_deref(),
            Some("https://example.com/docs/guide/other.html")
        );
        assert_eq!(
            resolve_url(base, "../up.html").as_deref(),
            Some("https://example.com/docs/up.html")
        );
        assert_eq!(resolve_url(base, "/root").as_deref(), Some("https://example.com/root"));
        assert_eq!(resolve_url(base, "//cdn.example/x").as_deref(), Some("https://cdn.example/x"));
        assert_eq!(
            resolve_url(base, "https://other.example/y").as_deref(),
            Some("https://other.example/y")
        );
        assert_eq!(resolve_url(base, "#anchor"), None);
        assert_eq!(resolve_url(base, "mailto:a@b"), None);
    }

    /// The refusal that stops a URL from a board file naming a local path.
    #[test]
    fn only_http_urls_split() {
        for refused in ["file:///etc/passwd", "ftp://example.com/x", "mailto:a@b", "/etc/passwd", ""]
        {
            assert_eq!(split_url(refused), None, "{refused} was accepted");
        }
        let parts = split_url("HTTPS://User:pass@Example.COM:8443/a/b?q=1#frag").unwrap();
        assert_eq!(parts.scheme, "https");
        assert_eq!(parts.host, "example.com:8443", "credentials must be dropped, host lowercased");
        assert_eq!(parts.path, "/a/b?q=1", "the fragment is never sent");
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// The cap is the refusal for a body that never ends, and it has to clear the biggest head
    /// this repo has measured — YouTube's, which closes at byte 692,232 — with the article
    /// still behind it.
    #[test]
    #[expect(
        clippy::assertions_on_constants,
        reason = "both sides are const *today*, which is the whole point: this is a guard \
                  on the constant, and it fires the moment somebody lowers the cap below \
                  the largest head this repo has measured. Deleting it because it cannot \
                  fail right now would remove the only thing that makes the number checkable"
    )]
    fn the_byte_cap_clears_the_largest_measured_head_and_is_still_a_cap() {
        assert!(DEFAULT_MAX_BYTES > 692_232 * 2, "a page's body must fit behind its head");
        assert!(DEFAULT_MAX_BYTES <= 8 * 1024 * 1024, "still a cap, not a crawler");
    }

    /// Clipping happens on a character boundary and says that it happened. A truncation that
    /// looked like the end of the article is the failure this reports its way out of.
    #[test]
    fn clipping_cuts_on_a_character_boundary_and_says_so() {
        let (short, cut) = clip("hello".to_owned(), 10);
        assert_eq!(short, "hello");
        assert!(!cut);

        let cjk = "\u{4e2d}".repeat(100);
        let (clipped, cut) = clip(cjk, 10);
        assert!(cut);
        assert!(clipped.starts_with(&"\u{4e2d}".repeat(10)));
        assert!(clipped.contains("truncated by Velm"));
        // The real assertion: it is still valid UTF-8 with whole characters in it.
        assert_eq!(clipped.chars().take(10).collect::<String>(), "\u{4e2d}".repeat(10));
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    /// A **fabricated** fixture in the shape measured against the live endpoint: the title
    /// anchor, the snippet anchor, and the `/l/?uddg=` redirect wrapper whose `&` arrives as
    /// `&amp;`. Real capture data is not pasted into this repository.
    const RESULTS: &str = r#"<div class="results">
  <div class="result results_links web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftorque%2Bspecs&amp;rut=deadbeef">Torque specs &amp; figures</a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftorque%2Bspecs&amp;rut=deadbeef">The <b>torque</b> table for every fastener.</a>
  </div>
  <div class="result results_links web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.net%2Fmanual&amp;rut=cafe">Manual</a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.net%2Fmanual&amp;rut=cafe">Chapter 3.</a>
  </div>
  <div class="result results_links web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="https://example.net/list?a=1&amp;b=2">Direct</a>
    </h2>
  </div>
  <a class="result--ad__a" href="//duckduckgo.com/y.js?ad=1">An advertisement</a>
</div>"#;

    /// The parse, and the thing about it that would silently be wrong: the redirect wrapper
    /// must be unwrapped, or every result looks as though it is hosted by the search engine
    /// and following one hands a tracking URL back to the site.
    #[test]
    fn duckduckgo_results_are_read_and_their_redirect_wrapper_removed() {
        let results = parse_html_results(RESULTS);
        assert_eq!(results.len(), 3, "an advertisement was counted as a result: {results:?}");

        assert_eq!(results[0].title, "Torque specs & figures");
        assert_eq!(
            results[0].url, "https://example.org/torque+specs",
            "the redirect wrapper survived"
        );
        assert!(!results[0].url.contains("rut="), "the tracking parameter survived");
        assert_eq!(results[0].snippet, "The torque table for every fastener.");

        assert_eq!(results[1].url, "https://example.net/manual");
        assert_eq!(results[1].snippet, "Chapter 3.");

        // A result with no snippet anchor keeps an empty snippet rather than borrowing the
        // next result's — the `last.snippet.is_empty()` guard is what makes that true.
        assert_eq!(results[2].title, "Direct");
        assert_eq!(results[2].snippet, "");
    }

    /// A result whose `href` is the page's own address still arrives as a **URL**, not as an
    /// attribute value: `&` is spelled `&amp;` in HTML, and `https://…?a=1&amp;b=2` is a
    /// different address from the one on the page — one whose second parameter is called
    /// `amp;b`. A/B'd: decoding after the query is split off leaves exactly that.
    #[test]
    fn a_direct_result_url_has_its_entities_decoded() {
        let results = parse_html_results(RESULTS);
        assert_eq!(results[2].url, "https://example.net/list?a=1&b=2");
        assert!(!results[2].url.contains("amp;"), "the entity survived into the URL");
    }

    /// A page with nothing in it yields nothing, which is what makes `search_at` able to tell
    /// "unreadable" from "empty" and report it rather than returning a silent empty list.
    #[test]
    fn a_page_with_no_results_yields_none_rather_than_guessing() {
        assert!(parse_html_results("<html><body>No results.</body></html>").is_empty());
        assert!(parse_html_results("").is_empty());
        assert!(parse_html_results("<a class=\"result__a\">no href</a>").is_empty());
    }

    #[test]
    fn searxng_json_is_read_and_a_result_with_no_url_is_dropped() {
        let json = r#"{"results":[
            {"title":"Torque","url":"https://example.org/t","content":"A blurb."},
            {"title":"No address","content":"dropped"},
            {"url":"https://example.net/u","snippet":"the other field name"},
            {"title":"Not a web page","url":"ftp://example.com/x"}
        ]}"#;
        let results = parse_searxng_results(json);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].snippet, "A blurb.");
        assert_eq!(results[1].title, "https://example.net/u", "a missing title falls back to the url");
        assert_eq!(results[1].snippet, "the other field name");

        assert!(parse_searxng_results("not json at all").is_empty());
        assert!(parse_searxng_results("{}").is_empty());
    }

    /// The default is DuckDuckGo's HTML host, and `off` is a named refusal rather than an
    /// endpoint that quietly answers nothing.
    #[test]
    fn the_default_engine_is_the_measured_one_and_off_is_a_refusal() {
        assert_eq!(
            SearchEngine::default(),
            SearchEngine::Html { endpoint: DEFAULT_SEARCH_ENDPOINT.to_owned() }
        );
        assert!(DEFAULT_SEARCH_ENDPOINT.starts_with("https://html.duckduckgo.com/"));
        assert_eq!(SearchEngine::Off.endpoint(), None);
    }

    /// A query is encoded, not concatenated. Without this, a query with a `&` in it becomes two
    /// parameters and the search silently runs on half of what was asked.
    #[test]
    fn a_query_is_percent_encoded() {
        assert_eq!(percent_encode_query("torque specs"), "torque+specs");
        assert_eq!(percent_encode_query("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode_query("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(percent_decode("caf%C3%A9"), "caf\u{e9}");
        assert_eq!(percent_decode("%%zz"), "%%zz", "a malformed escape survives as itself");
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "\u{4e2d}\u{6587}");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    /// The user agent is the module's honesty claim, and it is asserted rather than trusted:
    /// it must name Velm and must not name a browser.
    #[test]
    fn the_user_agent_identifies_velm_and_impersonates_nobody() {
        assert!(DEFAULT_USER_AGENT.starts_with("Velm/"));
        assert!(DEFAULT_USER_AGENT.contains("github.com/ibrahimbisen/Velm"));
        for pretence in ["Mozilla", "Chrome", "Safari", "AppleWebKit", "Gecko", "Edg/"] {
            assert!(
                !DEFAULT_USER_AGENT.contains(pretence),
                "the user agent claims to be {pretence}"
            );
        }
    }

    /// `robots.txt` is honoured by default, and nothing in the shipping configuration turns it
    /// off. A test rather than a comment, because a default that quietly inverted would make
    /// every claim in this module's doc comment false.
    #[test]
    fn robots_is_honoured_by_default() {
        assert!(ResearchConfig::default().respect_robots);
        // The same claim for the host policy, and it is the more important of the two: the
        // URL it guards is chosen by a model, and the switch that lifts it is the user's.
        assert!(
            !ResearchConfig::default().allow_local_hosts,
            "the host policy is off by default, so a model can reach this machine"
        );
    }

    /// **The SSRF gate.** The only check that used to stand between a model-chosen URL and
    /// this machine was the *scheme*, which `http://127.0.0.1:11434/api/…` passes as happily
    /// as any other page. Every address here reaches something that answers with no
    /// credential: the user's local model, Velm's own IPC server, the router, and — the worst
    /// of them — cloud instance metadata at `169.254.169.254`, which hands out keys.
    ///
    /// Asserted on the pure halves, because the resolving half needs a resolver: the literal
    /// forms and the names are exactly what `check_host` consults first, and they are where
    /// every entry in the module's policy list is pinned.
    #[test]
    fn the_host_policy_refuses_this_machine_and_the_local_network() {
        let blocked = [
            "127.0.0.1",
            "127.13.9.4",
            "0.0.0.0",
            "10.0.0.7",
            "172.16.4.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::abcd",
            "fc00::1",
            // The spelling a policy usually forgets: the loopback address as IPv6.
            "::ffff:127.0.0.1",
            "localhost",
            "LOCALHOST",
            "db.localhost",
            "printer.local",
            "metadata.google.internal",
            "nas.home.arpa",
            // A trailing dot is a fully-qualified name and the same host.
            "localhost.",
        ];
        for host in blocked {
            assert!(
                blocked_by_name(host).is_some(),
                "{host} was allowed, and it reaches this machine or this network"
            );
        }

        // The other half, which matters just as much: ordinary public hosts are not refused.
        for host in ["example.com", "8.8.8.8", "172.32.0.1", "11.0.0.1", "100.128.0.1", "2606:4700::1111"] {
            assert!(blocked_by_name(host).is_none(), "{host} is public and was refused");
        }

        // The reason is a sentence an agent can act on, not a code.
        let reason = blocked_by_name("169.254.169.254").unwrap();
        assert!(reason.contains("metadata"), "the reason did not say what is there: {reason}");
    }

    /// The port and the brackets come off before the address is read.
    ///
    /// [`UrlParts::host`] is host **and** port, so a policy that read it raw would test
    /// `"127.0.0.1:11434"`, fail to parse it as an address, and let it through — which is the
    /// single most likely local address a model would name.
    #[test]
    fn a_host_is_read_without_its_port_or_its_brackets() {
        assert_eq!(hostname_of("example.com"), "example.com");
        assert_eq!(hostname_of("example.com:8080"), "example.com");
        assert_eq!(hostname_of("127.0.0.1:11434"), "127.0.0.1");
        assert_eq!(hostname_of("[::1]"), "::1");
        assert_eq!(hostname_of("[::1]:8080"), "::1");
        assert_eq!(hostname_of("[fe80::1]:443"), "fe80::1");
        // A bare v6 literal is not legal in a URL and does turn up in hand-written strings;
        // splitting it on the last colon would produce a name that is refused by accident.
        assert_eq!(hostname_of("::1"), "::1");
        // A colon that is not a port must not be mistaken for one.
        assert_eq!(hostname_of("example.com:notaport"), "example.com:notaport");

        // The join that matters: through `split_url`, as `check_host` actually reads it.
        let parts = split_url("http://127.0.0.1:11434/api/generate").unwrap();
        assert_eq!(parts.host, "127.0.0.1:11434");
        assert!(blocked_by_name(hostname_of(&parts.host)).is_some());
    }

    /// ⚠ **A redirect is a request to another host, so the policy is re-applied per hop.**
    /// The redirect loop already re-checked the *scheme* every hop and never the host, so a
    /// public page answering `302 Location: http://127.0.0.1:…` walked straight in.
    ///
    /// Driven through `resolve_url`, which is what the loop uses to build the next target —
    /// asserting on the hop's resolved URL is what makes this about the loop rather than
    /// about `blocked_by_name` a second time.
    #[test]
    fn a_redirect_into_the_local_network_is_refused_at_the_hop_that_reaches_it() {
        // Each `Location` as a redirecting server would send it: absolute, protocol-relative
        // and root-relative, which are the three forms `resolve_url` has to handle.
        let hops = [
            ("https://example.org/second", false),
            ("http://127.0.0.1:11434/api/tags", true),
            ("//169.254.169.254/latest/meta-data/", true),
            ("/harmless", true),
        ];
        let mut target = "https://example.com/start".to_owned();
        let mut verdicts = Vec::new();
        for (location, _) in hops {
            let next = resolve_url(&target, location).expect("the hop did not resolve");
            let parts = split_url(&next).expect("the hop was not an http(s) URL");
            verdicts.push(blocked_by_name(hostname_of(&parts.host)).is_some());
            target = next;
        }
        let expected: Vec<bool> = hops.iter().map(|(_, blocked)| *blocked).collect();
        assert_eq!(verdicts, expected, "a hop into the local network was not caught");
        // The last hop is the one worth naming: a *root-relative* redirect inherits the
        // previous hop's host, so once a chain is inside the network every later hop is too.
        assert_eq!(target, "http://169.254.169.254/harmless");

        // A refusal must not read as a network failure, or the agent retries it forever.
        let refused = ResearchError::BlockedHost {
            url: "http://127.0.0.1:11434/api/tags".into(),
            host: "127.0.0.1".into(),
            reason: "a loopback address (127.0.0.0/8) — this machine".into(),
        };
        let text = refused.to_string();
        assert!(text.contains("127.0.0.1"), "{text}");
        assert!(text.contains("public internet"), "{text}");
        let crate_wide: crate::AgentError = refused.into();
        assert!(
            matches!(crate_wide, crate::AgentError::Refused(_)),
            "a policy refusal arrived as something retryable: {crate_wide:?}"
        );
    }

    /// ⚠ **The gate is *called*, and that is what nothing checked.**
    ///
    /// Every assertion above is on `blocked_by_name`, `hostname_of` or `resolve_url` — pure
    /// functions that know the policy perfectly and are not the policy. Delete the
    /// `self.check_host(&target, &parts)?` line out of `fetch_at` and every one of them stays
    /// green while a model-chosen `http://127.0.0.1:11434/…` is fetched. That is the shape
    /// `opens_context_menu` and `import_to_new_board` both had in `CLAUDE.md`: written, tested,
    /// and reached by nothing.
    ///
    /// **It needs no network.** A literal loopback address is refused by name, before the
    /// resolver is consulted and long before a request is made — so this drives the real
    /// entry point, `Research::fetch_at`, and asserts on the error it hands back.
    #[test]
    fn a_model_chosen_local_url_is_refused_by_the_fetch_itself_and_not_only_by_the_policy() {
        let research = Research::new(ResearchConfig::default());
        let refused = research
            .fetch_at("http://127.0.0.1:11434/api/generate", Instant::now())
            .expect_err("the fetch reached this machine");
        assert!(
            matches!(refused, ResearchError::BlockedHost { .. }),
            "a local address was refused, but not by the host policy: {refused:?}"
        );
        // The advice in the message has to be an instruction somebody can follow, which for a
        // while it was not: the switch it names was set in one place in the source, as `false`.
        assert!(
            refused.to_string().contains(ALLOW_LOCAL_HOSTS_ENV),
            "the refusal advertises an escape hatch it does not name: {refused}"
        );

        // The metadata address by the same route — the one that hands out credentials.
        assert!(matches!(
            research.fetch_at("http://169.254.169.254/latest/meta-data/", Instant::now()),
            Err(ResearchError::BlockedHost { .. })
        ));

        // And with the user's own switch on, the policy stands aside — the half that makes
        // this a gate rather than a wall. It has to get *past* the gate to prove anything, so
        // this one does attempt a connection: to port 1 on loopback, which nothing listens on,
        // with a short timeout so a firewall that drops rather than refuses cannot stall the
        // suite. Either way the answer is a **network** error, which is only reachable below
        // the check that would otherwise have refused the host.
        //
        // ⚠ A scheme that is not http(s) would *not* have proved this: `split_url` refuses
        // that one line above `check_host`, so the assertion would hold with the policy still
        // in force — the vacuous shape this whole test exists to correct.
        let allowed = Research::new(ResearchConfig {
            allow_local_hosts: true,
            respect_robots: false,
            timeout: Duration::from_millis(250),
            ..ResearchConfig::default()
        });
        let outcome = allowed.fetch_at("http://127.0.0.1:1/x", Instant::now());
        assert!(
            matches!(outcome, Err(ResearchError::Network { .. })),
            "the user's own switch did not lift the host policy: {outcome:?}"
        );
    }

    /// The switch the refusal advertises is one the user can actually throw.
    ///
    /// `allow_local_hosts` was written in exactly one place — `Default::default()`, as `false` —
    /// so the only way to take the advice in [`ResearchError::BlockedHost`] was to edit this
    /// file and rebuild. The assertion is on the **constructor the running tool uses**, since
    /// that is where the field being unreachable actually bit.
    #[test]
    fn the_host_policy_can_be_lifted_by_the_user_without_recompiling_velm() {
        // The reading, which is where a flag like this goes wrong: **anything** that is not one
        // of the four spellings leaves the policy on, so `VELM_ALLOW_LOCAL_HOSTS=no` cannot
        // read as permission — which is what a naive "set to anything" flag would do with it.
        for value in ["1", "true", "TRUE", "yes", " on "] {
            assert!(flag_is_on(Some(value)), "{value:?} should turn the switch on");
        }
        for value in ["0", "no", "off", "false", "", "please", "no thanks"] {
            assert!(!flag_is_on(Some(value)), "{value:?} must not turn the switch on");
        }
        assert!(!flag_is_on(None), "unset must leave the host policy in force");

        // Asserted without writing the environment: a test binary runs its tests as threads in
        // one process, so `set_var` here is a write every other test can see.
        //
        // The wiring, then: whatever the environment currently says, `from_env` is what the
        // field follows — where it used to follow nothing at all, because the only place it
        // was ever written was `Default::default()`.
        let from_env = ResearchConfig::from_env();
        assert_eq!(
            from_env.allow_local_hosts,
            flag_is_on(std::env::var(ALLOW_LOCAL_HOSTS_ENV).ok().as_deref()),
            "the constructor the running tool uses does not read {ALLOW_LOCAL_HOSTS_ENV}"
        );
        // And the default is unchanged: the switch is opt-in, whatever route it arrives by.
        assert!(!ResearchConfig::default().allow_local_hosts);
    }

    /// **The robots exemption is an equality test, not a prefix.** `/robots.txt` is exempt
    /// because fetching the rules cannot be subject to them — and `starts_with` said the same
    /// about three paths that are not the rules file, each of which then skipped the robots
    /// check for the whole request.
    #[test]
    fn only_the_rules_file_itself_is_exempt_from_the_rules() {
        assert!(is_robots_path("/robots.txt"));
        // A query on the rules file is still the rules file.
        assert!(is_robots_path("/robots.txt?v=2"));
        assert!(is_robots_path("/robots.txt#top"));

        // None of these is the rules file, and each used to be exempt.
        assert!(!is_robots_path("/robots.txt.bak"), "a backup of the rules is not the rules");
        assert!(!is_robots_path("/robots.txt/../private/secret"));
        assert!(!is_robots_path("/robots.txtsecret"));
        assert!(!is_robots_path("/robots.txt/admin"));

        // The join, against the parser that produces the path: `split_url` keeps the query on
        // `path`, which is why the split is needed at all.
        assert_eq!(split_url("https://a.example/robots.txt?v=2").unwrap().path, "/robots.txt?v=2");
        assert_eq!(
            split_url("https://a.example/robots.txt.bak").unwrap().path,
            "/robots.txt.bak"
        );
    }

    /// A refusal carries the status code. An agent told "amazon.com answered 403" can find
    /// another source; an agent handed an empty page cannot.
    #[test]
    fn a_refusal_names_the_status_and_the_rule() {
        let refused =
            ResearchError::Refused { url: "https://example.com/x".into(), status: 403 };
        assert!(refused.to_string().contains("403"), "{refused}");

        let disallowed = ResearchError::RobotsDisallowed {
            url: "https://example.com/w/api.php".into(),
            host: "example.com".into(),
            rule: "Disallow: /w/".into(),
        };
        let text = disallowed.to_string();
        assert!(text.contains("robots.txt"), "{text}");
        assert!(text.contains("Disallow: /w/"), "{text}");

        // And it survives the trip into the crate-wide error without losing the number.
        let crate_wide: crate::AgentError = refused.into();
        assert!(crate_wide.to_string().contains("403"), "{crate_wide}");
    }
}
