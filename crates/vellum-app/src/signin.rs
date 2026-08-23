//! Signing in to a `velmd` server from the desktop, off the frame loop.
//!
//! *"update the app so that the signing in is optional in the settings and i have to give a
//! url sign in and password … so i want it to connect to the correct server not just any
//! server"*. This is the half that turns three typed fields into a session cookie
//! [`crate::sync`] can carry.
//!
//! # The wire
//!
//! ```text
//! POST {base}api/v1/session
//!   content-type: application/json
//!   {"username": "...", "password": "..."}
//!
//!   204, Set-Cookie: velm_session=... | __Host-velm_session=...
//! ```
//!
//! ⚠ **`content-type: application/json` is not decoration.** `velmd` checks it *before* it
//! parses the body and answers 415 otherwise, because a form-encoded POST is a request a page
//! on another site can make and a JSON one is not. That check is the server's whole CSRF
//! defence, so a client that omits the header is not merely sloppy — it is a client that
//! cannot sign in at all.
//!
//! # Why there is a thread here
//!
//! `ureq` blocks and `crate::sync`'s connect timeout is ten seconds. A synchronous POST on the
//! frame path would freeze the window for ten seconds on a mistyped address, which is the
//! exact input this feature exists to accept. So: one worker, one shot, one channel, drained
//! per frame — the same shape as [`crate::sync`] and for the same reason.
//!
//! It is a *one-shot* worker rather than a pool, and that is the whole of its lifecycle: it
//! is spawned by [`start`], it sends one [`SignInReply`], and it returns. Nothing here idles.
//!
//! # RULE ZERO
//!
//! This module opens no file, holds no `Path`, and has no board. The only durable
//! consequence of anything in it is that `crate::library` records an address and a username
//! in `library.json`, which is the file it already wrote. No `.vellum`, no `-wal`, no `-shm`
//! and no blob is reachable from here.
//!
//! # What is deliberately not here
//!
//! **No account creation.** Redeeming an invite code is the web page's job, so the Mac stays
//! out of the invite contract entirely. The Account page says so in one sentence.
//!
//! **No password is kept.** There is no keychain call and no file. The consequence is real
//! and is stated on [`SignIn`]: `velmd` forgets every session when it restarts, and a session
//! idles out after twelve hours, so the password is typed again on every launch, on every
//! server restart, and after half a day of not using it.

use std::io::Read;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use vellum_ui::Secret;

/// The most of an error body that is read.
///
/// The body of a refusal is one sentence from `velmd`. A wrong address can answer with
/// anything at all, so this is a bound on *that*, not on a legitimate reply — and it is read
/// on a machine that has run out of memory twice.
const MAX_BODY_BYTES: u64 = 4 * 1024;

/// A sign-in in flight.
///
/// Held on `ActiveState` while the request is out and dropped when the answer lands. Dropping
/// it while the worker is still going is safe and costs nothing: the worker's `send` fails,
/// and it returns.
///
/// ⚠ **Nothing here survives the process, and nothing here survives the server.** `velmd`
/// keeps its sessions in memory and forgets them on restart; they also idle out after twelve
/// hours. So the session this produces is good until the next server restart or the next long
/// gap, whichever comes first, and then the password has to be typed again. That is the cost
/// of storing no secret, and it is the argument for a keychain step later rather than never.
pub struct SignIn {
    inbound: Receiver<SignInReply>,
    /// Whether the answer has already been taken, so a second [`Self::drain`] on a channel
    /// whose sender has gone reports nothing rather than inventing a failure.
    finished: bool,
}

/// What the server said.
///
/// ⚠ **No derived `Debug`:** the `Ok` arm carries a session, which is a credential. Same rule
/// as [`crate::sync::Credential`] and [`crate::sync::SyncReply`], and for the same reason —
/// the promise is not the mechanism, the hand-written `Debug` is.
pub enum SignInReply {
    /// A session. `name` is whatever the server called its cookie.
    Ok { server: String, username: String, name: String, value: String },
    /// One sentence for the Account page. Never a status code on its own.
    Refused(String),
}

impl std::fmt::Debug for SignInReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok { server, username, name, .. } => f
                .debug_struct("Ok")
                .field("server", server)
                .field("username", username)
                .field("cookie", name)
                .field("value", &"<redacted>")
                .finish(),
            Self::Refused(detail) => f.debug_tuple("Refused").field(detail).finish(),
        }
    }
}

impl SignIn {
    /// The answer, once. `None` while the request is still out.
    ///
    /// Never blocks, so it is safe on the frame path — which is the point of the whole
    /// module. A worker that died without answering is reported rather than waited on for
    /// ever: without that arm the page would sit on *Signing in* until the app was restarted.
    pub fn drain(&mut self) -> Option<SignInReply> {
        if self.finished {
            return None;
        }
        match self.inbound.try_recv() {
            Ok(reply) => {
                self.finished = true;
                Some(reply)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Some(SignInReply::Refused(
                    "Velm could not finish signing in. Try again.".to_owned(),
                ))
            }
        }
    }
}

/// Starts one sign-in.
///
/// `base` must already have been through [`normalize_server`] — the caller does that first so
/// a mistyped address costs no thread and is reported on the same frame it was typed.
///
/// The password is moved into the worker and dropped there. It reaches exactly one place: the
/// body of one POST.
pub fn start(base: String, username: String, password: Secret) -> SignIn {
    let (answer, inbound) = channel::<SignInReply>();
    let endpoint = session_endpoint(&base);

    let spawned = std::thread::Builder::new().name("velm-signin".to_owned()).spawn(move || {
        let reply = post_session(&endpoint, base, username, &password);
        // The receiver is gone: the app quit, or the person pressed Sign out while this was
        // still in the air. Nothing to do, and nothing to report it to.
        let _ = answer.send(reply);
    });
    if let Err(error) = spawned {
        log::warn!("sign-in: the worker would not start ({error})");
        // The channel is dropped with `answer`, so `drain` answers through its
        // `Disconnected` arm on the next frame rather than sitting on *Signing in* for ever.
    }

    SignIn { inbound, finished: false }
}

/// `POST {base}api/v1/session`, on the worker thread.
///
/// Never returns an error type, for `crate::sync::post`'s reason: every failure ends the
/// same way — one sentence in front of the user — and a `Result` would invite an early `?` on
/// a path whose whole job is to always answer.
fn post_session(
    endpoint: &str,
    base: String,
    username: String,
    password: &Secret,
) -> SignInReply {
    #[derive(serde::Serialize)]
    struct Credentials<'a> {
        username: &'a str,
        password: &'a str,
    }

    let body = match serde_json::to_vec(&Credentials {
        username: username.as_str(),
        password: password.expose(),
    }) {
        Ok(body) => body,
        // Unreachable in practice — two `&str` always encode — but this path must not
        // `unwrap`: `[profile.release]` sets `panic = "abort"`, so a panic here would kill
        // the application rather than raise something the page could show.
        Err(error) => {
            log::warn!("sign-in: encoding the request failed ({error})");
            return SignInReply::Refused("Velm could not build the request.".to_owned());
        }
    };

    let http = crate::sync::agent();
    let response = match http
        // ⚠ velmd checks this header *before* it parses the body and answers 415 otherwise.
        // It is the server's CSRF defence, not a courtesy.
        .post(endpoint)
        .header("content-type", "application/json")
        .send(body)
    {
        Ok(response) => response,
        Err(error) => {
            log::warn!("sign-in: {endpoint} ({error})");
            return SignInReply::Refused(
                "Could not reach your server. Check the address.".to_owned(),
            );
        }
    };

    let status = response.status().as_u16();
    // Read before the body is consumed: `into_body` takes the response by value.
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();

    let mut text = String::new();
    let _ = response.into_body().into_reader().take(MAX_BODY_BYTES).read_to_string(&mut text);

    if (200..300).contains(&status) {
        let lines: Vec<&str> = cookies.iter().map(String::as_str).collect();
        return match session_from_set_cookie(&lines) {
            Some((name, value)) => SignInReply::Ok { server: base, username, name, value },
            None => SignInReply::Refused(
                "Your server signed you in, and it sent no session. Tell the person who runs \
                 it."
                    .to_owned(),
            ),
        };
    }
    describe(status, &text, retry_after)
}

/// The sign-in route under a base from [`normalize_server`].
///
/// The base always ends in `/`, which is what makes this a concatenation rather than a
/// join — and it is what lets a reverse proxy mount `velmd` at a subpath without this
/// function knowing.
fn session_endpoint(base: &str) -> String {
    format!("{base}api/v1/session")
}

/// A status and a body into one sentence.
///
/// The wording follows `web/signin.js` so the two clients agree about what happened. A person
/// who is told two different things by the browser and by the Mac has to work out which one is
/// lying, and this repository has already paid once for two clients disagreeing.
///
/// ⚠ **403 is folded into the 401 arm on purpose.** A server that distinguishes *"wrong
/// password"* from *"that account may not sign in"* by status code has told an attacker which
/// usernames exist. The shared sentence is what closes that, and the Mac must not undo it.
pub fn describe(status: u16, body: &str, retry_after: Option<u64>) -> SignInReply {
    let sentence = match status {
        401 | 403 => "That username and password do not match.".to_owned(),
        404 => "This server does not have accounts.".to_owned(),
        // The one status that is Velm's own fault rather than the person's, so it says what
        // to do rather than what went wrong.
        415 => "This server did not accept the request. Update Velm.".to_owned(),
        429 => match retry_after {
            Some(seconds) => format!("Too many sign in attempts. Wait {seconds} seconds."),
            None => "Too many sign in attempts. Wait a few minutes.".to_owned(),
        },
        500..=599 => "Your server answered, and something went wrong inside it.".to_owned(),
        _ => {
            // An unrecognised status quotes the server, the way a sync failure does, because
            // the alternative is a number the person can do nothing with. Truncated by
            // **characters**, never by bytes: a proxy in front of velmd can answer with
            // anything, and slicing a multi-byte character in half aborts the process.
            let quoted: String = body.trim().chars().take(200).collect();
            if quoted.is_empty() {
                format!("Your server answered {status}.")
            } else {
                format!("Your server answered {status}: {quoted}")
            }
        }
    };
    SignInReply::Refused(sentence)
}

/// The session out of every `Set-Cookie` line the answer carried.
///
/// ⚠ **The name is not fixed and must not be assumed.** `velmd` sets `__Host-velm_session`
/// when it runs behind HTTPS and `velm_session` when it does not, so hard-coding either
/// spelling breaks half the deployments. What was sent is what is echoed back.
///
/// An empty value is **not** a session: `velm_session=; Max-Age=0` is exactly how a server
/// signs somebody out, and reading it as a credential would leave the Mac believing it was
/// signed in with nothing.
pub fn session_from_set_cookie(lines: &[&str]) -> Option<(String, String)> {
    for line in lines {
        // Everything past the first `;` is attributes — `Path`, `Max-Age`, `SameSite`,
        // `HttpOnly` — and none of it is the value.
        let pair = line.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else { continue };
        let name = name.trim();
        let value = value.trim();
        if name != SESSION_COOKIE && name != SESSION_COOKIE_HOST_PREFIXED {
            continue;
        }
        if value.is_empty() {
            continue;
        }
        return Some((name.to_owned(), value.to_owned()));
    }
    None
}

/// What `velmd` calls its session cookie without HTTPS.
const SESSION_COOKIE: &str = "velm_session";
/// What it calls it behind HTTPS. The `__Host-` prefix is a rule the *browser* enforces; the
/// Mac only has to send back what it was given.
const SESSION_COOKIE_HOST_PREFIXED: &str = "__Host-velm_session";

/// An address as typed into a canonical base, or the reason it is not one.
///
/// # The Rust twin of `web/boards.js`'s `normalizeServer`, including the part that guesses
///
/// ⚠ **A missing scheme takes `https://`, exactly as the web client's does.** The web version
/// prepends the *page's* protocol; a desktop application has no page, and the two schemes it
/// can use are `http` and `https`, so `https` is the one to guess. That means
/// `boards.example.com` and `example.com:8787` are both accepted, which matters because the
/// person typing is a family member who will not type a scheme. Refusing what the browser
/// accepts would be the two-clients-disagree defect this repository has already paid for.
///
/// The `://` test runs **before** anything is parsed, which is what stops `example.com:8787`
/// being read as a scheme called `example.com` and losing its port.
///
/// # What is stripped, and why each one
///
/// - **Credentials.** `https://evil.example@real.example/` reads as `evil.example` to a
///   person and resolves to `real.example`.
/// - **Query and fragment.** A base is a place, not a request.
/// - **Every scheme but `http` and `https`.** There is nothing else this can talk to, and a
///   refusal here is cheaper than a request that cannot be made.
///
/// # The trailing slash is kept
///
/// It is what makes the result a *base*, so `{base}api/v1/session` resolves inside it rather
/// than beside it — which is what lets a reverse proxy mount `velmd` under a subpath. It is
/// also the form the web client stores, so the two agree byte for byte. `crate::sync`'s
/// `endpoint_for` trims it again on its own, so nothing doubles.
pub fn normalize_server(raw: &str) -> Result<String, &'static str> {
    const BAD: &str = "That is not a web address. It should look like https://boards.example.com";
    const SCHEME: &str = "A Velm server is reached over http or https.";

    let text = raw.trim();
    if text.is_empty() {
        return Err("Type the address of your Velm server.");
    }

    let (scheme, rest) = match split_scheme(text) {
        Some(split) => split,
        // No scheme at all. Guess https and keep going, exactly as the web client does.
        // Leading slashes are dropped so `//host` is `host` rather than an empty authority.
        None => ("https".to_owned(), text.trim_start_matches('/')),
    };
    if scheme != "http" && scheme != "https" {
        return Err(SCHEME);
    }

    // The authority runs to the first `/`, `?` or `#`; everything after it that is not the
    // path is discarded here rather than parsed.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    // The path stops at the query and the fragment. `find` on the *tail*, so a `?` inside
    // the authority is impossible by construction.
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = &tail[..path_end];

    // `rsplit_once`, not `split_once`: a password may contain an `@`, and the last one is the
    // separator. This is the line that stops `evil.example@real.example` reading as a host.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, after)| after);
    let (host, port) = split_host_port(host_port).ok_or(BAD)?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(BAD);
    }

    let host = host.to_ascii_lowercase();
    // A default port is dropped, which is what `new URL` does — so an address typed with
    // `:443` and the same address without it are one stored value rather than two that look
    // different and mean the same thing.
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = match port {
        Some(number) if number == default_port => String::new(),
        Some(number) => format!(":{number}"),
        None => String::new(),
    };

    let mut path = path.to_owned();
    if !path.ends_with('/') {
        path.push('/');
    }
    Ok(format!("{scheme}://{host}{port}{path}"))
}

/// Splits `scheme://rest` when there is a scheme, and answers `None` when there is not.
///
/// A scheme is a letter followed by letters, digits, `+`, `-` or `.`, per RFC 3986 — the same
/// alphabet the web client's regular expression uses, and case-insensitive for the same
/// reason. Anything else means the `:` belongs to a port.
fn split_scheme(text: &str) -> Option<(String, &str)> {
    let (head, rest) = text.split_once("://")?;
    let mut characters = head.chars();
    if !characters.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !characters.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }
    Some((head.to_ascii_lowercase(), rest))
}

/// `host` and its port, with an IPv6 literal kept whole.
///
/// ⚠ **The port has to be checked, not merely split off.** With no scheme typed,
/// `javascript:alert(1)` becomes `https://javascript:alert(1)`, whose authority splits into a
/// host of `javascript` and a "port" of `alert(1)`. A browser's `new URL` refuses that; so
/// does this, and that refusal is the whole reason the scheme guess above is safe.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    // An IPv6 literal is bracketed and its colons are not port separators. The brackets stay
    // on the host, because they are part of how it is written back into a URL.
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = &authority[..=close];
        let after = &authority[close + 1..];
        if after.is_empty() {
            return Some((host, None));
        }
        return Some((host, Some(after.strip_prefix(':')?.parse::<u16>().ok()?)));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port.parse::<u16>().ok()?))),
        None => Some((authority, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- the address ------------------------------------------------------------------

    /// **The correction this module was rewritten for.** `web/boards.js`'s `normalizeServer`
    /// prepends the page's protocol when no scheme is typed, so `example.com:8787` is
    /// accepted there and becomes `https://example.com:8787/`. A Mac that refused it would
    /// disagree with the browser about the same input, which is a defect this repository has
    /// already paid for once — and the person typing is a family member who will type a bare
    /// hostname.
    #[test]
    fn an_address_with_no_scheme_becomes_https_and_keeps_its_port() {
        assert_eq!(
            normalize_server("boards.example.com").as_deref(),
            Ok("https://boards.example.com/")
        );
        assert_eq!(
            normalize_server("example.com:8787").as_deref(),
            Ok("https://example.com:8787/"),
            "the port was read as part of a scheme and lost"
        );
        assert_eq!(
            normalize_server("//boards.example.com").as_deref(),
            Ok("https://boards.example.com/")
        );
    }

    /// The result is a **base**: it ends in `/` so `{base}api/v1/session` resolves inside it
    /// rather than beside it, and so it matches the form the web client stores byte for byte.
    #[test]
    fn the_result_is_a_base_with_a_trailing_slash() {
        assert_eq!(
            normalize_server("http://127.0.0.1:8787").as_deref(),
            Ok("http://127.0.0.1:8787/")
        );
        assert_eq!(
            normalize_server("https://boards.example.com/").as_deref(),
            Ok("https://boards.example.com/"),
            "a slash that was already there must not double"
        );
        assert_eq!(
            normalize_server("  https://boards.example.com  ").as_deref(),
            Ok("https://boards.example.com/"),
            "a pasted address carries whitespace"
        );
        // A subpath survives, which is what lets a reverse proxy mount velmd under one.
        assert_eq!(normalize_server("https://host/velm").as_deref(), Ok("https://host/velm/"));
        assert_eq!(session_endpoint("https://host/velm/"), "https://host/velm/api/v1/session");
    }

    /// `https://evil.example@real.example/` reads as `evil.example` to a person and resolves
    /// to `real.example`, and a query is a request rather than a place.
    #[test]
    fn credentials_and_a_query_are_stripped() {
        assert_eq!(
            normalize_server("https://a:b@host/x?y#z").as_deref(),
            Ok("https://host/x/")
        );
        assert_eq!(
            normalize_server("https://evil.example@real.example/").as_deref(),
            Ok("https://real.example/")
        );
    }

    /// There is nothing else this can talk to, and the guess above is only safe because of
    /// the port check underneath it: with no scheme, `javascript:alert(1)` would otherwise
    /// become `https://javascript:alert(1)` and parse.
    #[test]
    fn only_http_and_https_are_addresses() {
        assert!(normalize_server("javascript:alert(1)").is_err());
        assert!(normalize_server("ftp://host").is_err());
        assert!(normalize_server("file:///Users/somebody").is_err());
        assert!(normalize_server("").is_err());
        assert!(normalize_server("   ").is_err());
        assert!(normalize_server("https://").is_err(), "no host at all");
        assert!(normalize_server("host:notaport").is_err());
        assert!(normalize_server("https://host:99999").is_err(), "a port is 16 bits");
        // Every refusal says something a person can act on, rather than a code. No em dash in
        // any of them: the user removed every one from the interface on purpose.
        for bad in ["javascript:alert(1)", "", "https://", "host:notaport", "ftp://host"] {
            let Err(reason) = normalize_server(bad) else { panic!("{bad:?} was accepted") };
            assert!(reason.len() > 20, "{reason}");
            assert!(!reason.contains('—'), "{reason}");
        }
    }

    /// The scheme and the host are compared case-insensitively everywhere else, so they are
    /// stored the way everything else spells them.
    #[test]
    fn the_scheme_and_the_host_are_lowered() {
        assert_eq!(
            normalize_server("HTTPS://Boards.Example.COM").as_deref(),
            Ok("https://boards.example.com/")
        );
    }

    /// A default port is dropped, so an address typed with `:443` and the same one without it
    /// are one stored value rather than two that look different and mean the same.
    #[test]
    fn a_default_port_is_dropped_and_any_other_is_kept() {
        assert_eq!(normalize_server("https://host:443/").as_deref(), Ok("https://host/"));
        assert_eq!(normalize_server("http://host:80/").as_deref(), Ok("http://host/"));
        assert_eq!(normalize_server("https://host:8787/").as_deref(), Ok("https://host:8787/"));
        // An IPv6 literal keeps its brackets and its colons.
        assert_eq!(normalize_server("http://[::1]:8787").as_deref(), Ok("http://[::1]:8787/"));
        assert_eq!(normalize_server("http://[::1]").as_deref(), Ok("http://[::1]/"));
    }

    // ----- the cookie -------------------------------------------------------------------

    /// velmd names its cookie `__Host-velm_session` behind HTTPS and `velm_session`
    /// otherwise, so a hard-coded spelling breaks half the deployments. What was sent is what
    /// goes back.
    #[test]
    fn a_session_is_read_under_either_cookie_name() {
        assert_eq!(
            session_from_set_cookie(&["velm_session=abc; Path=/; HttpOnly"]),
            Some(("velm_session".to_owned(), "abc".to_owned()))
        );
        assert_eq!(
            session_from_set_cookie(&["__Host-velm_session=abc; Path=/; Secure; HttpOnly"]),
            Some(("__Host-velm_session".to_owned(), "abc".to_owned()))
        );
    }

    /// Everything past the first `;` is attributes. Reading one of them as the value would
    /// send `Path` back as a session id, which the server would refuse for ever.
    #[test]
    fn only_the_value_before_the_first_semicolon_is_the_session() {
        let found = session_from_set_cookie(&[
            "__Host-velm_session=s3ss10n; Path=/; Max-Age=43200; SameSite=Lax; Secure",
        ]);
        assert_eq!(found, Some(("__Host-velm_session".to_owned(), "s3ss10n".to_owned())));
    }

    /// `velm_session=; Max-Age=0` is exactly how a server signs somebody out. Reading it as a
    /// credential would leave the Mac believing it was signed in with nothing, and every
    /// later sync would answer 401.
    #[test]
    fn an_empty_session_value_is_not_a_session() {
        assert_eq!(session_from_set_cookie(&["velm_session=; Max-Age=0"]), None);
        assert_eq!(session_from_set_cookie(&[]), None);
        assert_eq!(session_from_set_cookie(&["theme=dark; Path=/"]), None, "not ours");
        assert_eq!(session_from_set_cookie(&["velm_session"]), None, "no `=` at all");
    }

    /// A server may set more than one cookie in one answer, and the session is not
    /// necessarily first.
    #[test]
    fn the_session_is_found_among_other_cookies() {
        let found = session_from_set_cookie(&[
            "consent=1; Path=/",
            "theme=dark; Path=/; Max-Age=99",
            "__Host-velm_session=s3ss10n; Path=/; Secure",
        ]);
        assert_eq!(found, Some(("__Host-velm_session".to_owned(), "s3ss10n".to_owned())));
    }

    // ----- the sentences ----------------------------------------------------------------

    /// ⚠ **The account-enumeration oracle.** A server that distinguishes *"wrong password"*
    /// from *"that account may not sign in"* by status code has told an attacker which
    /// usernames exist. `web/signin.js` closes that with a shared sentence; the Mac must read
    /// the same, or the browser and the desktop disagree about what happened.
    #[test]
    fn a_wrong_password_and_a_refused_account_read_the_same() {
        let SignInReply::Refused(wrong) = describe(401, "no", None) else { panic!("401 is a refusal") };
        let SignInReply::Refused(refused) = describe(403, "no", None) else {
            panic!("403 is a refusal")
        };
        assert_eq!(wrong, refused);
        assert!(!wrong.contains("401") && !wrong.contains("403"), "the status leaked: {wrong}");
    }

    /// Each sentence names what to do next, and none of them is a bare number. A person told
    /// "429" can do nothing; a person told how long to wait can wait.
    #[test]
    fn every_refusal_is_a_sentence_a_person_can_act_on() {
        let cases = [
            (404, None, "accounts"),
            (415, None, "Update Velm"),
            (429, Some(90), "90 seconds"),
            (429, None, "few minutes"),
            (500, None, "inside it"),
            (503, None, "inside it"),
        ];
        for (status, retry_after, expected) in cases {
            let SignInReply::Refused(sentence) = describe(status, "", retry_after) else {
                panic!("{status} is a refusal")
            };
            assert!(sentence.contains(expected), "{status}: {sentence}");
            assert!(sentence.ends_with('.'), "{status}: {sentence}");
            // No em dashes in anything a person reads: the user removed every one on purpose.
            assert!(!sentence.contains('—'), "{status}: {sentence}");
        }
    }

    /// An unrecognised status quotes the server, because the alternative is a number nobody
    /// can act on — and the quote is truncated by **characters**, never by bytes. With
    /// `panic = "abort"` in the release profile, slicing a multi-byte character in half would
    /// not raise an error, it would kill the application.
    #[test]
    fn an_unrecognised_status_quotes_the_server_without_slicing_a_character() {
        let SignInReply::Refused(sentence) = describe(418, "  I am a teapot\n", None) else {
            panic!("418 is a refusal")
        };
        assert!(sentence.contains("418") && sentence.contains("I am a teapot"), "{sentence}");

        let long: String = "é".repeat(400);
        let SignInReply::Refused(sentence) = describe(418, &long, None) else { panic!() };
        assert!(sentence.chars().count() < 250, "the quote is unbounded");

        let SignInReply::Refused(empty) = describe(418, "", None) else { panic!() };
        assert!(empty.contains("418") && empty.ends_with('.'), "{empty}");
    }

    /// The `Ok` arm carries a session, which is a credential, so the same rule that gives
    /// `Credential` and `SyncReply` hand-written `Debug`s applies here.
    #[test]
    fn a_reply_does_not_print_the_session() {
        let reply = SignInReply::Ok {
            server: "https://boards.example.com/".to_owned(),
            username: "sam".to_owned(),
            name: "__Host-velm_session".to_owned(),
            value: "a-real-session".to_owned(),
        };
        let printed = format!("{reply:?}");
        assert!(!printed.contains("a-real-session"), "{printed}");
        assert!(printed.contains("sam") && printed.contains("__Host-velm_session"), "{printed}");
    }

    /// A worker that never answered must not leave the page on *Signing in* for the life of
    /// the process. Driven through the channel the worker writes to, so this exercises
    /// `drain`'s own bookkeeping rather than a setter written for the test.
    #[test]
    fn a_worker_that_dies_is_reported_rather_than_waited_on() {
        let (answer, inbound) = channel::<SignInReply>();
        let mut pending = SignIn { inbound, finished: false };
        assert!(pending.drain().is_none(), "nothing has come back yet");
        drop(answer);
        let Some(SignInReply::Refused(sentence)) = pending.drain() else {
            panic!("a dead worker answered nothing")
        };
        assert!(sentence.ends_with('.'), "{sentence}");
        assert!(pending.drain().is_none(), "the answer is taken exactly once");
    }

    /// The happy path's bookkeeping, from the other side.
    #[test]
    fn an_answer_is_delivered_once() {
        let (answer, inbound) = channel::<SignInReply>();
        let mut pending = SignIn { inbound, finished: false };
        answer
            .send(SignInReply::Ok {
                server: "https://host/".to_owned(),
                username: "sam".to_owned(),
                name: "velm_session".to_owned(),
                value: "abc".to_owned(),
            })
            .expect("the receiver is right there");
        assert!(matches!(pending.drain(), Some(SignInReply::Ok { .. })));
        assert!(pending.drain().is_none(), "the answer is taken exactly once");
    }
}
