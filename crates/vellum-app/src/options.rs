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
        }
    }
}

/// What `main` should do after parsing.
#[derive(Debug, PartialEq)]
pub enum Command {
    Run(Box<Options>),
    ShowHelp,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut options = Options::default();
    let mut args = args.into_iter();

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
            other => bail!("unknown option `{other}`\n\n{HELP}"),
        }
    }

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
