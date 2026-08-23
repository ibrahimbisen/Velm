//! Command-line options.
//!
//! Hand-parsed rather than pulled through `clap`: the binary's size is a product
//! requirement — `docs/01-architecture.md` §3 budgets a 10–15 MB installer against
//! Miro's 295 MB — and a parser this small can be tested exhaustively, which a
//! derive macro's behaviour cannot.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const HELP: &str = "\
velm — infinite canvas

USAGE:
    velm [OPTIONS]

BOARD:
    --board PATH       Open (or create) this .vellum board. Defaults to the board
                       in Velm's data directory.
    --open PATH        Open another board in its own tab. May be repeated; the last
                       one is the tab in front.
    --tab N            Bring tab N to the front once everything is open. 0 is the
                       board library, which is always the first tab.
    --import PATH      Import a saved Miro clipboard payload (.html) on startup.
    --rtb PATH         A Miro .rtb backup, or a FOLDER of them, to join image,
                       document and link-card preview bytes against. Repeatable.
                       Archives in the app's own `archives/` folder load anyway.
                       Also used by Cmd+V.
    --bench N          Skip the stored board and generate N stickies in memory.
    --seed N           Seed for the bench board, so runs are comparable (default 1).

VIEW:
    --zoom PERCENT     Open at this zoom instead of fitting the board.
    --hud              Show the performance HUD from the first frame (F1 toggles).

SYNC:
    --sync-server URL  Keep the open board in step with a velmd server, both ways.
                       The bearer token comes from $VELM_SYNC_TOKEN, never from a
                       flag: an argument is visible to every process on the machine
                       through `ps`, and this one guards every board on the server.
    --sync-every SECS  How often to ask (default 3). A failure backs off on its own.

INPUT:
    --pan-sensitivity F    Multiplies scroll and drag panning (default 1.0).
    --zoom-sensitivity F   Multiplies wheel and pinch zoom (default 1.0).
    --invert-pan           Flip both scroll axes.
    --invert-zoom          Flip scroll and pinch zoom direction.
    --wheel-pans           Mouse wheel pans instead of zooming.
    --no-inertia           Disable the glide after a drag-pan.

DIAGNOSTICS:
    --exit-after SECS  Quit after SECS and log the average frame rate.
    --select-all       Select every item on the first frame (diagnostic).
    --select-one       Select a single item on the first frame, so the resize and
                       rotate handles are on screen for `--screenshot`.
    --open-dialog NAME Raise a dialog on the first frame so `--screenshot` can
                       photograph it: shortcuts, documentation, about,
                       new-board, import-steps.
    --show NAME        Raise a chrome surface only a click can otherwise reach, so
                       `--screenshot` can photograph it. Repeatable.
                       Panels:  properties, palette, find
                       Flyouts: shapes, pen, eraser, more
                       Menu:    menu (the ⋮ popup; a named group inside it cannot
                                be forced open — see `menu::force_open_menu`)
    --paste            Paste the system clipboard on the first frame, so the flavour
                       handling can be checked without a hand on the keyboard.
    --demo NAME        Make the open board into a fixture, or drive a live gesture that
                       `--screenshot` cannot photograph. Each reports a verdict as a
                       toast and a log line, so an unattended run can see a pass.
                       Boards:   readme, shapes, empty, table, chart, mindmap, kanban,
                                 links
                       Gestures: card-drag, typing, caret, connector, placing, snapping,
                                 object-eraser, group-handles, locked-arrange,
                                 widget-edit, copy-paste, context-menu,
                                 edit-then-delete
                       Cost:     reproject-cost — what one edit does to the open
                                 board: the rebuild, sampled, and how many of its
                                 cached layouts a one-item change throws away.
                       Menu rows with no other unattended path:
                                 export-svg, export-pdf, export-png, present
    --screenshot PATH  Write one composited frame — board, glass and chrome — to a
                       PNG and carry on. Rendered offscreen rather than read from
                       the swapchain, so it works with the window behind another.
    --no-vsync         Present without waiting for the display. Reports renderer
                       headroom instead of the refresh rate; benchmarking only.
    -h, --help         Show this message.

CONTROLS (Miro's, so muscle memory transfers):
    left drag                  marquee select
    space + drag, middle drag  pan
    right drag                 pan
    right click                everything the selection can do — or the canvas's own
                               menu when it lands on bare board
    two-finger scroll          pan, tracking the fingers
    scroll wheel               zoom about the pointer
    pinch, ctrl/cmd + scroll   zoom about the pointer
    V / H                      select tool / hand tool
    N / T / S / F              sticky, text, shape, frame
    P / E / C                  pen, eraser, connector
    Cmd+V                      paste a Miro board
    Cmd+A, Cmd+Z, Cmd+Shift+Z  select all, undo, redo
    Cmd+K / Cmd+F              commands / find
    Delete                     delete the selection
    Shift+1 / Shift+2          fit the board / fit the selection
    0                          zoom to 100%
    F1                         toggle the performance HUD
    Esc                        clear the selection
    Cmd+Q                      quit

Everything else is in the menus beside the board's name — Board, Edit, View and
Preferences — and Cmd+K lists every command with its shortcut.
";

/// Everything the command line can set.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// The board file to open. `None` uses [`crate::editor::default_board_path`].
    pub board: Option<PathBuf>,
    /// Further boards to open, each in its own tab, in the order given.
    ///
    /// The window can hold several boards at once — see `crate::session` — and until
    /// this existed there was no way to *start* it in that state, which meant the one
    /// honest check of the chrome (`--screenshot`, because a window behind another
    /// window is never presented to) could never photograph more than one tab.
    pub open: Vec<PathBuf>,
    /// Bring this tab to the front once every board has opened. `0` is the board
    /// library, which is always the first tab.
    pub tab: Option<usize>,
    /// A Miro clipboard capture to import once, at startup.
    pub import: Option<PathBuf>,
    /// Miro `.rtb` backups — the highest-quality source of image, document and
    /// link-card preview bytes. Each entry is a file or a folder of them.
    ///
    /// A list rather than one path because a migration is not one board: bringing 58
    /// boards across means 58 backups, one paste carries one board's widgets, and a
    /// single archive fixed at launch would supply assets for exactly one of them.
    pub rtb: Vec<PathBuf>,
    /// Generate this many stickies in memory instead of opening a stored board.
    /// Zero opens the board.
    pub bench_items: usize,
    pub seed: u64,
    /// Initial zoom as a scale factor, or `None` to fit the board's content.
    pub zoom: Option<f64>,
    pub hud: bool,
    pub pan_sensitivity: f64,
    pub zoom_sensitivity: f64,
    pub invert_pan: bool,
    pub invert_zoom: bool,
    /// Make a mouse wheel pan instead of zoom. The default is to zoom:
    /// a wheel has one axis and coarse notches, which makes panning with it
    /// slow and one-directional.
    pub wheel_pans: bool,
    pub inertia: bool,
    /// Quit after this many seconds. Exists so a benchmark or a CI smoke test can
    /// run the real event loop unattended and still terminate.
    pub exit_after_secs: Option<f64>,
    /// Select everything on the first frame, so the properties panel is up.
    ///
    /// A diagnostic, not a feature. An idle board with nothing selected and an idle
    /// board with the panel open are different programs — the second runs the whole
    /// inspector every frame — and only the first was ever measurable unattended.
    /// That gap is why a memory fault the user could reproduce in seconds survived
    /// several rounds of unattended measurement.
    pub select_all: bool,
    /// `--select-one`: select exactly one item on the first frame.
    ///
    /// A diagnostic, like `select_all`. Handles are drawn only for a single selection,
    /// so this is the only way an unattended run can photograph them.
    pub select_one: bool,
    /// `--open-dialog NAME`: raise a modal on the first frame.
    ///
    /// A diagnostic, for the same reason as `select_all`. A dialog exists only between
    /// the click that opens it and the click that dismisses it, so `--screenshot` —
    /// which renders one frame of an untouched window — could never photograph one, and
    /// the chrome that is hardest to get right was the only chrome nothing could check.
    pub open_dialog: Option<String>,
    /// `--show NAME`: raise chrome that only a click can otherwise reach.
    ///
    /// The same diagnostic argument as `open_dialog`, for the surfaces it does not cover: the
    /// properties panel, the command palette, the find bar, the four tool flyouts and one
    /// group of the `⋮` menu are all click-only state, so `--screenshot` — one frame of a
    /// window nobody is touching — could photograph none of them. That is most of the chrome
    /// `docs/04-ui-reference.md` §4 specifies, and it was the part with no unattended check
    /// at all.
    ///
    /// A list rather than one name, because these compose: a properties panel *and* an open
    /// menu is a legitimate state and comparing Velm against a Miro screenshot often needs
    /// two surfaces up at once.
    pub show: Vec<String>,
    /// `--paste`: run one paste on the first frame.
    ///
    /// A diagnostic. What ⌘V does depends entirely on what the machine's clipboard is
    /// holding at that instant, which no unattended run can otherwise arrange — so the
    /// path the user actually reported broken was the one path nothing could exercise.
    pub paste: bool,
    /// `--demo NAME`: populate the board with a fixture on the first frame.
    ///
    /// A diagnostic. The shape catalogue is 41 forms across two rendering paths — an
    /// SDF for the ones whose parameters survive a non-uniform scale, a tessellated
    /// mesh for the rest — and placing them by hand to see whether they all draw is
    /// forty-one drags.
    pub demo: Option<String>,
    /// Write one composited frame to this PNG and keep running.
    ///
    /// The swapchain is not readable and a window behind another window is not
    /// presented to at all, so a smoke test that only watches the frame counter
    /// cannot tell a working composite from one that draws nothing. This renders the
    /// same three stages into an offscreen target and reads it back, which is the
    /// only unattended check of the chrome and the glass that is worth anything.
    pub screenshot: Option<PathBuf>,
    pub no_vsync: bool,
    /// `--sync-server URL`: keep the open board in step with a `velmd` server.
    ///
    /// `None` — the default — is the whole application as it was: no thread, no socket, no
    /// request. That matters more than it looks. A sync that is on by default is a program
    /// that talks to the network the first time it opens somebody's boards, and every
    /// argument in `CLAUDE.md` for why link previews ship *on* runs the other way here:
    /// a preview fills in a card, and a sync writes to a board.
    pub sync: Option<SyncOptions>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            board: None,
            open: Vec::new(),
            tab: None,
            import: None,
            rtb: Vec::new(),
            bench_items: 0,
            seed: 1,
            zoom: None,
            hud: false,
            pan_sensitivity: 1.0,
            zoom_sensitivity: 1.0,
            invert_pan: false,
            invert_zoom: false,
            wheel_pans: false,
            inertia: true,
            exit_after_secs: None,
            select_all: false,
            select_one: false,
            open_dialog: None,
            show: Vec::new(),
            paste: false,
            demo: None,
            screenshot: None,
            no_vsync: false,
            sync: None,
        }
    }
}

/// Where to sync the open board, and how often.
///
/// One server and one cadence, applied to whichever board is in front. Per-board servers
/// were considered and are not built: a person owns one velmd, and a second address is a
/// second place their boards can be, which is the thing this whole feature exists to stop.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncOptions {
    /// The server's base address, e.g. `https://boards.example.com`. No trailing slash is
    /// required; `crate::sync::Sync::new` builds the path.
    pub server: String,
    /// Seconds between round trips.
    pub period: f64,
}

/// The environment variable the bearer token is read from.
pub const TOKEN_VAR: &str = "VELM_SYNC_TOKEN";

/// The sync token, or `None` if the environment does not carry one.
///
/// ⚠ **An environment variable and deliberately not a flag.** Every process on this machine
/// can read another's argv through `ps`, and this token is the only thing between a stranger
/// and ~58 boards that cannot be re-imported. `velmd` reads its own copy from `$VELMD_TOKEN`
/// for exactly the same reason, so the two halves of the pair are consistent.
///
/// Outside [`parse_args`] on purpose, which is what keeps that function pure: it is covered
/// by tests that pass fixed argument lists, and a `std::env::var` inside it would make every
/// one of them depend on the shell that happened to run them.
///
/// An empty or blank value answers `None` rather than an empty token — `export
/// VELM_SYNC_TOKEN=` is how a shell unsets a variable by accident, and an empty bearer token
/// is refused by the server as an authentication failure, which reads as the wrong problem.
pub fn sync_token() -> Option<String> {
    std::env::var(TOKEN_VAR).ok().map(|value| value.trim().to_owned()).filter(|t| !t.is_empty())
}

/// Where the token is kept once it has been taken out of the environment.
///
/// A `OnceLock` rather than a field on [`Options`], because `Options` derives `Debug` and a
/// secret in a derived `Debug` is a secret in whatever log line ever formats it — the same
/// reasoning that gave `SyncReply` a hand-written one.
static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// [`sync_token`], and then **take it out of this process's environment**.
///
/// # ⚠ Why an environment variable is not automatically the safer choice
///
/// A flag was rejected because argv is readable by every process on the machine through `ps`.
/// That is true, and on its own it makes an environment variable look like the answer — but
/// an environment is *inherited*, and this application's job includes launching third-party
/// binaries: `claude`, `codex`, `gemini`, and a PTY the user can type into. Nothing in the
/// tree calls `env_clear`, so every one of them could read `$VELM_SYNC_TOKEN` directly. The
/// mechanism chosen to avoid an attacker who can run `ps` was handing the secret to the
/// attacker this application invites in on purpose.
///
/// Removing it after the one read closes that: the value is already in memory where it is
/// needed, and a child spawned later inherits an environment that no longer carries it.
///
/// # Safety
///
/// `remove_var` is unsafe because another thread reading the environment concurrently is a
/// data race, and `getenv` is called by more things than it looks — SQLite's temporary-file
/// lookup among them.
///
/// ⚠ **This must be called from `main`, before the event loop, and the first version was not.**
/// It claimed to run "before the agent runtime, the link pool, the sync worker or any autosave
/// writer exists" — from `ActiveState::new`, by which point `Editor::open` has already started
/// the autosave *writer thread* and `Appearance::watch` its poller. The argument was the whole
/// safety case and the argument was false. It is the argument, not a check, that makes an
/// `unsafe` sound, so the fix is to make the argument true rather than to soften it.
///
/// Called once. A second call is a no-op that answers the same value, so a caller cannot lose
/// the token by asking twice, and nothing has to remember the ordering.
///
/// # Why the environment was not automatically safer than a flag
///
/// A flag was rejected because argv is readable by every process through `ps`. True — but an
/// environment is **inherited**, and this application's job includes launching third-party
/// binaries: `claude`, `codex`, `gemini`, and a PTY the user can type into. Nothing in the
/// tree calls `env_clear`, so every one of them could read `$VELM_SYNC_TOKEN` directly. The
/// mechanism chosen to defeat an attacker who can run `ps` was handing the secret to the
/// attacker this application invites in on purpose.
///
/// ⚠ It is taken **unconditionally**, not only when `--sync-server` was given. The startup
/// warning tells people to export it from their shell profile, so a launch *without* the flag
/// is the common case — and that is exactly the launch that would otherwise hand it to every
/// agent. The cost is that the variable is gone from this process; it is untouched in the
/// shell that set it.
pub fn take_sync_token() -> Option<String> {
    TOKEN
        .get_or_init(|| {
            let token = sync_token();
            // SAFETY: `main` calls this before the event loop is built and before any board
            // is opened, so this process is single-threaded here. See the note above for why
            // that has to be asserted at the call site rather than hoped for.
            unsafe { std::env::remove_var(TOKEN_VAR) };
            token
        })
        .clone()
}

/// Whether a server address is this machine, so a missing token is a legitimate setup
/// rather than one that will be refused on every request.
///
/// Deliberately conservative: anything it cannot recognise answers `false`, which produces a
/// warning about a configuration that may be fine. The other direction — calling a public
/// address loopback — is silence about ~58 boards behind no gate at all, and this function
/// exists precisely to make that noise.
///
/// It is a string test, not a DNS lookup. A name that *resolves* to 127.0.0.1 is not
/// recognised, and that is the right answer for a warning: the token still has to be right
/// for whatever the name reaches today, and resolving here would mean a network call inside
/// the launch path.
pub fn is_loopback(server: &str) -> bool {
    // The host is between the scheme and the first `/`, `?` or `#`, minus any port and any
    // `user@`. Taken apart by hand rather than with a URL crate: this is the only URL this
    // application parses, and the answer is a warning.
    let rest = server.split_once("://").map_or(server, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, after)| after);
    // An IPv6 literal is bracketed, and its colons are not a port separator.
    let host = if let Some(inner) = host.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();

    if host == "localhost" || host.ends_with(".localhost") || host == "::1" || host == "0:0:0:0:0:0:0:1" {
        return true;
    }
    // The whole 127.0.0.0/8 block, not just 127.0.0.1 — `127.0.0.2` is equally this machine
    // and a check for the one familiar spelling would warn about a correct setup.
    host.parse::<std::net::Ipv4Addr>().is_ok_and(|address| address.is_loopback())
}

/// Seconds between round trips, when nobody says otherwise.
///
/// Three seconds. Fast enough that moving a sticky on the iPad appears on the Mac before you
/// have looked away from it, slow enough that an idle board is twenty requests a minute
/// rather than a thousand. A failure backs off on its own from here.
///
/// It was a `const` inside [`parse_args`] and is `pub` now because sync can be turned on from
/// Settings ▸ Account as well as from a flag, and a second cadence written out beside this
/// one is two numbers that drift apart.
pub const DEFAULT_SYNC_PERIOD: f64 = 3.0;

/// What `main` should do after parsing.
#[derive(Debug, PartialEq)]
pub enum Command {
    Run(Box<Options>),
    ShowHelp,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    let mut sync_server: Option<String> = None;
    let mut sync_period = DEFAULT_SYNC_PERIOD;

    while let Some(arg) = args.next() {
        // `--flag=value` and `--flag value` both work; users type both, and
        // rejecting one is a papercut with no upside.
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (arg, None),
        };

        let mut value = || -> Result<String> {
            inline
                .clone()
                .or_else(|| args.next())
                .with_context(|| format!("{name} needs a value"))
        };

        match name.as_str() {
            "-h" | "--help" => return Ok(Command::ShowHelp),
            "--board" => options.board = Some(PathBuf::from(value()?)),
            // Repeatable, so it appends rather than replacing: every other path flag
            // here names one thing, and this one names a list.
            "--open" => options.open.push(PathBuf::from(value()?)),
            "--tab" => options.tab = Some(whole(&name, &value()?)?),
            "--import" => options.import = Some(PathBuf::from(value()?)),
            "--rtb" => options.rtb.push(PathBuf::from(value()?)),
            "--bench" => options.bench_items = whole(&name, &value()?)?,
            "--seed" => options.seed = whole(&name, &value()?)?,
            "--zoom" => {
                let raw = value()?;
                let percent = positive(&name, &raw)?;
                options.zoom = Some(percent / 100.0);
            }
            "--hud" => options.hud = true,
            "--pan-sensitivity" => options.pan_sensitivity = positive(&name, &value()?)?,
            "--zoom-sensitivity" => options.zoom_sensitivity = positive(&name, &value()?)?,
            "--invert-pan" => options.invert_pan = true,
            "--invert-zoom" => options.invert_zoom = true,
                "--wheel-pans" => options.wheel_pans = true,
            "--no-inertia" => options.inertia = false,
            "--exit-after" => options.exit_after_secs = Some(positive(&name, &value()?)?),
            "--select-all" => options.select_all = true,
            "--select-one" => options.select_one = true,
            "--open-dialog" => options.open_dialog = Some(value()?),
            "--show" => options.show.push(value()?),
            "--paste" => options.paste = true,
            "--demo" => options.demo = Some(value()?),
            "--screenshot" => options.screenshot = Some(PathBuf::from(value()?)),
            "--no-vsync" => options.no_vsync = true,
            // Collected as two locals and assembled below, so the two flags can arrive in
            // either order. Written straight into `options.sync` instead, `--sync-every`
            // before `--sync-server` would either be lost or would have to invent a server.
            "--sync-server" => sync_server = Some(value()?),
            "--sync-every" => sync_period = positive(&name, &value()?)?,
            other => bail!("unknown option `{other}`\n\n{HELP}"),
        }
    }

    // ⚠ `--sync-every` alone is *not* an error and is deliberately not one: it says how
    // often, and a cadence with nowhere to send is simply unused. Refusing it would make
    // `--sync-every 5` fail on a machine that has not set a server yet, which is the
    // configuration somebody arrives at while setting one up.
    if let Some(server) = &sync_server {
        // ⚠ **A scheme, or nothing works and the reason is invisible.** `ureq` cannot resolve
        // `localhost:8787/api/v1/...` to an absolute http(s) URI, so a schemeless address
        // fails every request with a URI-parse sentence that names neither the flag nor the
        // fix. Worse, `is_loopback` answers *true* for it, so the missing-token warning does
        // not fire either — the one message that would have pointed at the real problem is
        // suppressed by the same mistake. Caught here, at the door, where the sentence can
        // name what to type.
        anyhow::ensure!(
            server.starts_with("http://") || server.starts_with("https://"),
            "--sync-server needs a full address beginning http:// or https://, got `{server}`\n\
             For a server on this machine:      --sync-server http://127.0.0.1:8787\n\
             For one on the internet:           --sync-server https://boards.example.com"
        );
    }
    options.sync = sync_server.map(|server| SyncOptions { server, period: sync_period });

    Ok(Command::Run(Box::new(options)))
}

fn whole<T: std::str::FromStr>(flag: &str, raw: &str) -> Result<T> {
    raw.parse()
        .map_err(|_| anyhow::anyhow!("{flag} expects a whole number, got `{raw}`"))
}

/// A finite, strictly positive number. Every numeric flag here is a rate, a
/// duration or a scale, and zero is nonsense for all three — accepting it would
/// produce a frozen camera or an event loop that exits before its first frame.
fn positive(flag: &str, raw: &str) -> Result<f64> {
    let value: f64 = raw
        .parse()
        .with_context(|| format!("{flag} expects a number, got `{raw}`"))?;
    if !(value.is_finite() && value > 0.0) {
        bail!("{flag} expects a positive number, got `{raw}`");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Options {
        match parse_args(args.iter().map(|a| (*a).to_string())) {
            Ok(Command::Run(options)) => *options,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    /// The default is **no sync at all**, and that is the assertion worth having: a sync
    /// that arrived by default is a program that talks to the network the first time it
    /// opens somebody's boards.
    #[test]
    fn sync_is_off_unless_a_server_is_named() {
        assert_eq!(run(&["--hud"]).sync, None);
    }

    /// Both orders, because the two flags are collected as locals and assembled at the end
    /// precisely so that `--sync-every` before `--sync-server` is not lost. Written straight
    /// into `options.sync` instead, the first of these would pass and the second would
    /// silently take the default cadence.
    #[test]
    fn the_two_sync_flags_compose_in_either_order() {
        let after = run(&["--sync-server", "https://boards.example.com", "--sync-every", "10"]);
        let before = run(&["--sync-every=10", "--sync-server=https://boards.example.com"]);
        assert_eq!(after.sync, before.sync);
        let sync = after.sync.expect("a server was named");
        assert_eq!(sync.server, "https://boards.example.com");
        assert!((sync.period - 10.0).abs() < f64::EPSILON, "period was {}", sync.period);
    }

    /// ⚠ A schemeless address is refused at the door, and both halves of why are the point:
    /// `ureq` cannot build a request from it, **and** `is_loopback` answers `true` for it — so
    /// the missing-token warning is suppressed by the same mistake that breaks every request,
    /// and the user gets a URI-parse error naming neither.
    #[test]
    fn a_server_address_must_carry_its_scheme() {
        for good in ["http://127.0.0.1:8787", "https://boards.example.com"] {
            assert!(run(&["--sync-server", good]).sync.is_some(), "{good} should be accepted");
        }
        for bad in ["localhost:8787", "boards.example.com", "127.0.0.1:8787", "ftp://x"] {
            let refused = parse_args(["--sync-server".to_string(), bad.to_string()]);
            let message = refused.expect_err("{bad} must be refused").to_string();
            assert!(message.contains("http://"), "the message must say what to type: {message}");
        }
    }

    /// A cadence with nowhere to send is unused, not an error — someone setting this up
    /// types one flag before the other, and failing there fails the launch over a
    /// half-finished configuration.
    #[test]
    fn a_cadence_with_no_server_is_not_an_error() {
        assert_eq!(run(&["--sync-every", "5"]).sync, None);
    }

    /// ⚠ The token must not be reachable from an argument. Every process on the machine can
    /// read another's argv through `ps`, and this token is the only thing between a stranger
    /// and boards that cannot be re-imported — so `--sync-token` is expected to be rejected
    /// as an unknown option, exactly as any other misspelling would be.
    #[test]
    fn the_token_cannot_be_passed_as_an_argument() {
        let refused = parse_args(["--sync-token".to_string(), "hunter2".to_string()]);
        let message = refused.expect_err("--sync-token must not be accepted").to_string();
        assert!(message.contains("unknown option"), "{message}");
    }

    /// The warning about a missing token fires on a public address and stays quiet on this
    /// machine. Both halves matter: silence about a public server is silence about every
    /// board behind it, and noise about `127.0.0.1` trains people to ignore the warning.
    #[test]
    fn loopback_is_recognised_in_the_spellings_people_actually_type() {
        for here in [
            "http://127.0.0.1:8787",
            "http://127.0.0.1",
            // The whole 127/8 block is this machine, not only the familiar spelling.
            "http://127.0.0.2:8787/",
            "http://localhost:8787",
            "https://LOCALHOST",
            "http://[::1]:8787",
            "http://user@localhost:8787/api",
            // ⚠ Not `localhost:8787` — `is_loopback` still answers true for a schemeless
            // address, and that is *why* `parse_args` refuses one outright. Asserting it here
            // as a supported spelling is what made the combination invisible: the warning
            // that would have named the missing token is suppressed by the same mistake that
            // makes every request fail.
        ] {
            assert!(is_loopback(here), "{here} should be loopback");
        }
        for away in [
            "https://boards.example.com",
            "http://192.168.1.10:8787",
            // ⚠ The nastiest of these: a hostname that merely *starts* with the digits.
            "http://127.0.0.1.example.com",
            "https://example.com/127.0.0.1",
            "http://[2001:db8::1]:8787",
            "",
        ] {
            assert!(!is_loopback(away), "{away} should not be loopback");
        }
    }

    fn parse(args: &[&str]) -> Result<Command> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    fn options(args: &[&str]) -> Options {
        match parse(args).unwrap() {
            Command::Run(options) => *options,
            Command::ShowHelp => panic!("expected options, got help"),
        }
    }

    /// With no arguments the app opens the user's board. It used to generate a
    /// synthetic one, which is the wrong default for something that is now an editor
    /// rather than a rendering demo.
    #[test]
    fn no_arguments_opens_the_stored_board() {
        let o = options(&[]);
        assert_eq!(o, Options::default());
        assert_eq!(o.board, None);
        assert_eq!(o.bench_items, 0, "the default must not be a synthetic board");
        assert!(o.inertia);
    }

    #[test]
    fn flags_accept_both_spellings() {
        assert_eq!(options(&["--bench", "100000"]).bench_items, 100_000);
        assert_eq!(options(&["--bench=100000"]).bench_items, 100_000);
        assert_eq!(
            options(&["--board=/tmp/x.vellum"]).board,
            Some(PathBuf::from("/tmp/x.vellum"))
        );
    }

    #[test]
    fn board_import_and_archive_are_paths() {
        let o = options(&[
            "--board",
            "/boards/engine.vellum",
            "--import",
            "/captures/board.html",
            "--rtb",
            "/backups/backup.rtb",
        ]);
        assert_eq!(o.board, Some(PathBuf::from("/boards/engine.vellum")));
        assert_eq!(o.import, Some(PathBuf::from("/captures/board.html")));
        assert_eq!(o.rtb, vec![PathBuf::from("/backups/backup.rtb")]);
    }

    /// `--open` is the one repeatable flag: it names a list, and every other path flag
    /// names one thing. A second `--board` still replaces the first, which is what
    /// makes the two read differently at a glance.
    #[test]
    fn open_appends_a_tab_per_occurrence_and_tab_picks_one() {
        let o = options(&[
            "--board=/boards/a.vellum",
            "--open",
            "/boards/b.vellum",
            "--open=/boards/c.vellum",
            "--tab",
            "2",
        ]);
        assert_eq!(o.board, Some(PathBuf::from("/boards/a.vellum")));
        assert_eq!(
            o.open,
            vec![PathBuf::from("/boards/b.vellum"), PathBuf::from("/boards/c.vellum")],
            "the order is the order the tabs open in"
        );
        assert_eq!(o.tab, Some(2));

        assert!(options(&[]).open.is_empty());
        assert_eq!(options(&[]).tab, None);
        // Tab zero is the board library, so it has to survive a parser that treats a
        // zero as "not given".
        assert_eq!(options(&["--tab=0"]).tab, Some(0));
        assert_eq!(
            options(&["--board=/a", "--board=/b"]).board,
            Some(PathBuf::from("/b")),
            "--board still names one board"
        );
    }

    #[test]
    fn view_and_input_flags_combine() {
        let o = options(&[
            "--bench",
            "50000",
            "--seed=9",
            "--zoom",
            "100",
            "--hud",
            "--pan-sensitivity=1.5",
            "--zoom-sensitivity",
            "0.5",
            "--invert-pan",
            "--invert-zoom",
            "--no-inertia",
            "--exit-after",
            "3.5",
            "--no-vsync",
        ]);
        assert_eq!(o.bench_items, 50_000);
        assert_eq!(o.seed, 9);
        assert_eq!(o.zoom, Some(1.0));
        assert!(o.hud && o.invert_pan && o.invert_zoom && o.no_vsync);
        assert!(!o.inertia);
        assert_eq!(o.pan_sensitivity, 1.5);
        assert_eq!(o.zoom_sensitivity, 0.5);
        assert_eq!(o.exit_after_secs, Some(3.5));
    }

    /// Zoom is typed as a percentage, the way it is shown in the HUD, and stored as
    /// the scale factor the camera uses.
    #[test]
    fn zoom_is_a_percentage_on_the_way_in() {
        assert_eq!(options(&["--zoom", "25"]).zoom, Some(0.25));
        assert_eq!(options(&["--zoom=400"]).zoom, Some(4.0));
        assert!(parse(&["--zoom", "0"]).is_err());
        assert!(parse(&["--zoom", "-50"]).is_err());
    }

    /// A zero sensitivity is a camera that cannot move, which reads as a frozen app
    /// rather than as a mistyped flag.
    #[test]
    fn sensitivities_must_be_positive() {
        assert!(parse(&["--pan-sensitivity", "0"]).is_err());
        assert!(parse(&["--zoom-sensitivity", "-1"]).is_err());
        assert!(parse(&["--pan-sensitivity", "nan"]).is_err());
    }

    #[test]
    fn help_short_circuits() {
        assert_eq!(parse(&["--help"]).unwrap(), Command::ShowHelp);
        assert_eq!(parse(&["-h"]).unwrap(), Command::ShowHelp);
        assert_eq!(parse(&["--bench", "10", "--help"]).unwrap(), Command::ShowHelp);
    }

    #[test]
    fn a_missing_value_is_an_error_not_a_default() {
        let err = parse(&["--bench"]).unwrap_err().to_string();
        assert!(err.contains("--bench needs a value"), "got: {err}");
        assert!(parse(&["--board"]).is_err());
    }

    #[test]
    fn a_malformed_value_names_the_flag_and_the_input() {
        let err = format!("{:#}", parse(&["--bench", "lots"]).unwrap_err());
        assert!(err.contains("--bench"), "got: {err}");
        assert!(err.contains("lots"), "got: {err}");
    }

    #[test]
    fn a_nonsense_duration_is_rejected() {
        assert!(parse(&["--exit-after", "0"]).is_err());
        assert!(parse(&["--exit-after", "-1"]).is_err());
        assert!(parse(&["--exit-after", "nan"]).is_err());
    }

    #[test]
    fn an_unknown_flag_shows_the_usage() {
        let err = parse(&["--turbo"]).unwrap_err().to_string();
        assert!(err.contains("--turbo"), "got: {err}");
        assert!(err.contains("USAGE"), "the error should show usage: {err}");
    }

    /// The help text is the only documentation of the bindings a user sees, and the
    /// bindings are the thing this build exists to fix. A binding that stops being
    /// listed is a binding nobody finds.
    #[test]
    fn the_help_documents_every_binding_that_moves_the_camera() {
        for binding in [
            "marquee select",
            "space + drag",
            "middle drag",
            "two-finger scroll",
            "pinch",
            "scroll wheel",
            "right drag",
            "Cmd+V",
            "Cmd+K",
            "F1",
        ] {
            assert!(HELP.contains(binding), "the help never mentions `{binding}`");
        }
    }

    /// Every flag the parser accepts has to appear in the help, or it is a feature
    /// only its author knows about — and every documented flag has to parse.
    #[test]
    fn every_flag_is_documented_and_accepted() {
        const TAKES_VALUE: [&str; 9] = [
            "--board",
            "--import",
            "--rtb",
            "--bench",
            "--seed",
            "--zoom",
            "--pan-sensitivity",
            "--zoom-sensitivity",
            "--screenshot",
        ];
        const SWITCHES: [&str; 5] = [
            "--hud",
            "--invert-pan",
            "--invert-zoom",
            "--no-inertia",
            "--no-vsync",
        ];

        for flag in TAKES_VALUE {
            assert!(HELP.contains(flag), "`{flag}` is undocumented");
            assert!(parse(&[flag, "1"]).is_ok(), "`{flag}` is documented but not accepted");
            assert!(parse(&[flag]).is_err(), "`{flag}` accepted a missing value");
        }
        for flag in SWITCHES {
            assert!(HELP.contains(flag), "`{flag}` is undocumented");
            assert!(parse(&[flag]).is_ok(), "`{flag}` is documented but not accepted");
        }
        assert!(HELP.contains("--exit-after"));
        assert!(parse(&["--exit-after", "1"]).is_ok());
    }
}
