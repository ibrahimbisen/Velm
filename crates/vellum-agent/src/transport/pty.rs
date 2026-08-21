//! A CLI agent in a real pseudo-terminal.
//!
//! `docs/07-agent-canvas.md` §5b. This is Maestri's mechanic and it exists for two things
//! the protocol transports cannot do: an agent with **no protocol mode** still runs, and one
//! agent can **literally type into another's session** ([`AgentTransport::write_input`]).
//!
//! A pty rather than piped stdio, because the difference is visible in the output: a program
//! that detects a pipe turns off colour, turns off progress, and often turns off the
//! interactive behaviour that makes it worth watching at all. The agent believes it is
//! talking to a terminal, so it emits what it would for a human.
//!
//! # What this module does to the bytes, and what it deliberately does not
//!
//! [`Ansi`] is a **line-oriented** terminal, not a screen. It resolves the escape sequences
//! that change what a line *says* — SGR colour, erase-line, carriage-return overwrite, tabs,
//! backspace, cursor-column moves — and drops the ones that only make sense against a grid.
//! A full-screen TUI is therefore lossy here, and that is the trade: a transcript is a
//! scrolling record, and a faithful screen would mean re-emitting the whole grid on every
//! repaint, which floods the file that has to survive an overnight run.
//!
//! Colour is parsed and then **discarded at the event boundary**, because
//! `TranscriptEvent::Terminal` carries text and no styling. The parse still earns its place
//! twice over: it is what removes the escape bytes correctly (a regex over `\x1b\[[0-9;]*m`
//! misses every other sequence a real CLI emits), and [`Line::runs`] keeps the styling for a
//! painter that wants it. Carrying colour *through* the event stream would be a change to
//! `transcript.rs`, which every consumer reads.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize,
    native_pty_system,
};

use crate::provider::Transport as TransportKind;
use crate::transcript::{TranscriptEvent, TurnId, TurnOutcome};
use crate::transport::{AgentTransport, LaunchSpec, probe_command};
use crate::{AgentError, Result};

/// The terminal a session gets when the caller does not say. Wide enough that a coding
/// agent's diffs and tables are not wrapped into nonsense before we ever see them.
pub const DEFAULT_SIZE: (u16, u16) = (120, 30);

/// How many completed lines the transport keeps for the painter.
///
/// **The bound that matters.** Events leave through the channel and the app drains them
/// every frame, but this buffer is retained by the transport itself — so without a cap a
/// process printing in a loop would grow Velm's memory until the machine gave out, which on
/// an 8GB machine that has kernel-panicked twice is not a hypothetical. Two thousand lines
/// is more than a terminal window shows and about a megabyte at worst.
pub const MAX_SCROLLBACK: usize = 2_000;

/// The longest line the terminal will hold before forcing a break.
///
/// The second half of the memory bound: a process that writes megabytes with no newline —
/// a `cat` of a minified bundle, a hung progress bar — would otherwise grow one line without
/// limit. The line is **broken rather than truncated**, so the characters are all still there;
/// what is lost is only where the writer thought the line ended.
///
/// ⚠ **A break resets the column to zero**, which is the part any caller of [`Ansi::put`] in a
/// loop has to account for: a loop whose stop was worked out from the column *before* the
/// break never reaches it. That is not hypothetical — the tab arm of [`Ansi::ground`] diverged
/// on it, and that is a hang rather than a wrong character.
pub const MAX_LINE_CELLS: usize = 4_096;

/// The most parameter and intermediate bytes one CSI sequence may accumulate.
///
/// The third part of the memory bound. A CSI sequence ends at its final byte (0x40–0x7E) and
/// nothing guarantees a child sends one — `\x1b[` followed by an endless run of digits is a
/// vector that grows for as long as the process runs. Real sequences are a handful of bytes;
/// the longest anything here reads is `38;2;r;g;b`, so 256 is two orders of margin.
pub const MAX_CSI_PARAMETER_BYTES: usize = 256;

/// How much is read from the terminal at once.
const READ_CHUNK: usize = 8 * 1024;

/// The environment variable a PTY agent's own configuration can read the resolved system
/// context from.
///
/// A terminal has no system-prompt slot. Typing the context into the session would be worse
/// than doing nothing — the shell would try to *run* it — so it is offered here and the
/// agent picks it up or does not. This is the one place a PTY session is less capable than
/// the other two transports, and it is stated rather than papered over.
pub const CONTEXT_ENV: &str = "VELM_SYSTEM_CONTEXT";

// ---------------------------------------------------------------------------------------
// The terminal
// ---------------------------------------------------------------------------------------

/// What a run of characters looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// An ANSI palette index, 0–15 for the named colours and up to 255 for the cube. `None`
    /// is the terminal's default, which is the theme's own ink.
    pub foreground: Option<u8>,
    pub background: Option<u8>,
}

/// One styled span of a line, as **byte** offsets into [`Line::text`].
///
/// Bytes rather than characters because the painter slices the string with them, and they
/// are recorded while the string is built — so `text[run.start..run.start + run.len]` is a
/// valid slice by construction rather than by arithmetic anyone has to trust. This codebase
/// has aborted twice on a byte index that was computed instead of observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: usize,
    pub len: usize,
    pub style: Style,
}

/// One completed line of terminal output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// An `ESC` was seen; the next byte says what kind of sequence this is.
    Escape,
    /// Inside `ESC [ … final`.
    Csi,
    /// Inside `ESC ] … BEL` or `ESC ] … ESC \` — a window title, which we drop.
    Osc,
    /// An `ESC` inside an OSC string: `ESC \` ends it, anything else continues.
    OscEscape,
}

/// A line-oriented ANSI terminal.
///
/// Feed it bytes; take completed lines out. **Incremental by construction**: escape
/// sequences and multi-byte characters both survive being split across two `feed` calls,
/// which is not a nicety — a read boundary lands wherever the kernel says, and a UTF-8
/// character decoded per chunk turns into `U+FFFD` in perfectly good output.
#[derive(Debug)]
pub struct Ansi {
    cells: Vec<(char, Style)>,
    column: usize,
    style: Style,
    state: State,
    /// Raw parameter and intermediate bytes of the CSI sequence being read.
    parameters: Vec<u8>,
    /// The tail of a UTF-8 character split across a read boundary.
    partial_utf8: Vec<u8>,
    finished: Vec<Line>,
}

/// The flag Velm's MCP server is handed to a wrapped CLI with.
///
/// The same spelling `claude_cli` uses, and used here under the same probe: a terminal agent
/// may be any binary the user named, and most of them have never heard of it.
const MCP_CONFIG_FLAG: &str = "--mcp-config";

impl Default for Ansi {
    fn default() -> Self {
        Self::new()
    }
}

impl Ansi {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            column: 0,
            style: Style::default(),
            state: State::Ground,
            parameters: Vec::new(),
            partial_utf8: Vec::new(),
            finished: Vec::new(),
        }
    }

    /// Consumes a chunk of terminal output.
    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            match self.state {
                State::Ground => self.ground(*byte),
                State::Escape => self.escape(*byte),
                State::Csi => self.csi(*byte),
                State::Osc => match byte {
                    0x07 => self.state = State::Ground,
                    0x1b => self.state = State::OscEscape,
                    _ => {}
                },
                State::OscEscape => {
                    self.state = if *byte == b'\\' { State::Ground } else { State::Osc };
                }
            }
        }
    }

    /// Takes the lines completed so far.
    pub fn take_lines(&mut self) -> Vec<Line> {
        std::mem::take(&mut self.finished)
    }

    /// The line being written, as it currently stands. Not yet in [`Ansi::take_lines`].
    pub fn current(&self) -> Line {
        render(&self.cells)
    }

    /// Ends the line in progress, if there is one. For end of stream.
    pub fn flush(&mut self) {
        if !self.cells.is_empty() {
            self.newline();
        }
    }

    fn ground(&mut self, byte: u8) {
        if byte >= 0x80 {
            // A continuation or leading byte of a multi-byte character. Held until it makes
            // a whole one — `from_utf8` distinguishes "not finished yet" from "invalid" by
            // whether `error_len` is set, which is the entire reason this is incremental.
            self.partial_utf8.push(byte);
            match std::str::from_utf8(&self.partial_utf8) {
                Ok(text) => {
                    let characters: Vec<char> = text.chars().collect();
                    for character in characters {
                        self.put(character);
                    }
                    self.partial_utf8.clear();
                }
                Err(error) if error.error_len().is_none() => {
                    // Still incomplete. Four bytes is the longest a character can be, so
                    // anything longer is broken input rather than a slow arrival.
                    if self.partial_utf8.len() >= 4 {
                        self.partial_utf8.clear();
                        self.put(char::REPLACEMENT_CHARACTER);
                    }
                }
                Err(_) => {
                    self.partial_utf8.clear();
                    self.put(char::REPLACEMENT_CHARACTER);
                }
            }
            return;
        }

        if !self.partial_utf8.is_empty() {
            // An ASCII byte cannot continue a character, so whatever was pending is broken.
            self.partial_utf8.clear();
            self.put(char::REPLACEMENT_CHARACTER);
        }

        match byte {
            b'\n' => self.newline(),
            // Carriage return puts the cursor back at column zero **without** clearing:
            // what follows overwrites in place. This is how every progress bar and spinner
            // in every CLI works, and treating it as a newline is what turns one of them
            // into a thousand lines of transcript.
            b'\r' => self.column = 0,
            // ⚠ **The stop is recomputed against what `put` did, not against what it was
            // asked to do.** [`Ansi::put`] breaks the line at [`MAX_LINE_CELLS`] and a break
            // puts the column back to **zero** — so a target worked out before the break is
            // never reached and `while self.column < next` runs for ever, pushing a full
            // 4,096-cell line into `finished` every 4,096 turns. `feed` never returns: the
            // node goes dead, memory grows without bound, and `MAX_SCROLLBACK` never gets a
            // chance to prune because it is downstream of a call that does not come back.
            //
            // It is not a corner. `\x1b[4096G` clamps the column to `MAX_LINE_CELLS - 1`
            // (see the `b'G'` arm), which is *inside* the window a tab diverges in — and
            // 4,090 ordinary printed characters followed by a tab reaches it with no escape
            // sequence at all.
            //
            // A column that did not advance is a line that wrapped, and a tab stop belongs to
            // the line it was written on: stopping there ends the tab where the line ended,
            // which is what a terminal does.
            b'\t' => {
                let next = (self.column / 8 + 1) * 8;
                while self.column < next {
                    let before = self.column;
                    self.put(' ');
                    if self.column <= before {
                        break;
                    }
                }
            }
            0x08 => self.column = self.column.saturating_sub(1),
            0x1b => self.state = State::Escape,
            // Bell, and every other C0 control we have no line-oriented meaning for.
            _ if byte < 0x20 || byte == 0x7f => {}
            _ => self.put(byte as char),
        }
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.parameters.clear();
                self.state = State::Csi;
            }
            b']' => self.state = State::Osc,
            // `ESC =`, `ESC >`, `ESC M`, `ESC c`… keypad modes, reverse index, reset. None
            // has a line-oriented meaning; the byte is consumed so it cannot reach the text.
            _ => self.state = State::Ground,
        }
    }

    fn csi(&mut self, byte: u8) {
        // Parameter bytes 0x30–0x3F and intermediates 0x20–0x2F accumulate; 0x40–0x7E ends
        // the sequence and says what it was.
        //
        // **Bounded**, for the same reason the line is: a sequence's final byte is whatever
        // the child sends, and a child that never sends one — `\x1b[` followed by megabytes
        // of digits — would otherwise grow this vector without limit. Past the cap the extra
        // bytes are *dropped rather than ending the sequence*: a real CSI is a handful of
        // bytes, so anything this long is not one, and treating byte 257 as the final byte
        // would execute an arbitrary command chosen by the noise.
        if (0x20..0x40).contains(&byte) {
            if self.parameters.len() < MAX_CSI_PARAMETER_BYTES {
                self.parameters.push(byte);
            }
            return;
        }
        self.state = State::Ground;
        let parameters = self.numbers();
        match byte {
            b'm' => self.sgr(&parameters),
            // Erase in line. 0 (or absent): from the cursor to the end; 1: the start of the
            // line up to and including the cursor; 2: the whole line.
            b'K' => match parameters.first().copied().unwrap_or(0) {
                1 => {
                    let upto = (self.column + 1).min(self.cells.len());
                    for cell in &mut self.cells[..upto] {
                        *cell = (' ', self.style);
                    }
                }
                2 => self.cells.clear(),
                _ => self.cells.truncate(self.column.min(self.cells.len())),
            },
            // Erase in display. There is no display to erase, so the honest line-oriented
            // reading is "the partial line is about to be repainted" — drop it.
            b'J' => {
                self.cells.clear();
                self.column = 0;
            }
            // Cursor position / horizontal absolute. A repaint is starting; the line in
            // progress is discarded rather than emitted, because a TUI that homes the cursor
            // sixty times a second would otherwise write sixty copies of its own screen into
            // the transcript.
            b'H' | b'f' => {
                self.cells.clear();
                self.column = 0;
            }
            // ⚠ **Both arms clamp to [`MAX_LINE_CELLS`], and that clamp is a memory bound
            // rather than a fidelity choice.** The column comes off the wire as an arbitrary
            // `usize`, and [`Ansi::put`] pads with spaces up to it *before* it checks the
            // line cap — so `\x1b[1000000000Gx`, ten bytes from any child process, asks for
            // a billion sixteen-byte cells and aborts the application. The cap is already the
            // longest line this terminal will hold, so a column past it has nowhere to be.
            //
            // `MAX_LINE_CELLS - 1` rather than `MAX_LINE_CELLS`, because a column is an index:
            // the last cell of a full line is at `MAX_LINE_CELLS - 1`, and clamping one higher
            // would let `put` pad to the cap and then push one cell past it.
            //
            // ⚠ **What the clamp fixed and what it then made deterministic.** It does stop the
            // billion-cell allocation. It also lands every oversized column on 4,095 — one
            // cell short of a line break — so `\x1b[NG` for any N ≥ 4,089 followed by a **tab**
            // put the tab arm of [`Ansi::ground`] into the divergent window every time, turning
            // an abort into a hang. The tab arm is where that is answered; this is only where
            // the input arrives, and it is recorded here because the two are one bug.
            b'G' => {
                self.column = parameters
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(MAX_LINE_CELLS - 1);
            }
            // `saturating_add` before the clamp: the addition itself overflows on a parameter
            // near `usize::MAX`, and a wrapped column is a *small* number, which looks like
            // working output rather than like the bug it is.
            b'C' => {
                self.column = self
                    .column
                    .saturating_add(parameters.first().copied().unwrap_or(1).max(1))
                    .min(MAX_LINE_CELLS - 1);
            }
            b'D' => {
                self.column =
                    self.column.saturating_sub(parameters.first().copied().unwrap_or(1).max(1));
            }
            // Cursor up/down, scrolling regions, mode sets: nothing a line can express.
            _ => {}
        }
    }

    /// The CSI parameters as numbers. A missing parameter reads as 0, which is what every
    /// sequence here treats as its default.
    fn numbers(&self) -> Vec<usize> {
        // Private-mode sequences (`ESC [ ? 25 l`) carry a `?` that is not part of a number;
        // dropping it here means they fall through to the ignored arm rather than being
        // mistaken for a numbered one.
        let text: String = self
            .parameters
            .iter()
            .filter(|byte| byte.is_ascii_digit() || **byte == b';')
            .map(|byte| *byte as char)
            .collect();
        if text.is_empty() {
            return Vec::new();
        }
        text.split(';').map(|part| part.parse::<usize>().unwrap_or(0)).collect()
    }

    fn sgr(&mut self, parameters: &[usize]) {
        if parameters.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut index = 0;
        while index < parameters.len() {
            let code = parameters[index];
            match code {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.foreground = Some((code - 30) as u8),
                39 => self.style.foreground = None,
                40..=47 => self.style.background = Some((code - 40) as u8),
                49 => self.style.background = None,
                90..=97 => self.style.foreground = Some((code - 90 + 8) as u8),
                100..=107 => self.style.background = Some((code - 100 + 8) as u8),
                // Extended colour: `38;5;n` is a palette index, `38;2;r;g;b` is truecolor.
                // The extra parameters are consumed either way, or they would be read as
                // further SGR codes and paint the line at random.
                38 | 48 => {
                    let foreground = code == 38;
                    match parameters.get(index + 1).copied() {
                        Some(5) => {
                            let value = parameters.get(index + 2).copied().unwrap_or(0);
                            let value = u8::try_from(value).unwrap_or(u8::MAX);
                            if foreground {
                                self.style.foreground = Some(value);
                            } else {
                                self.style.background = Some(value);
                            }
                            index += 2;
                        }
                        Some(2) => index += 4,
                        _ => {}
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn put(&mut self, character: char) {
        while self.cells.len() < self.column {
            self.cells.push((' ', Style::default()));
        }
        if self.column < self.cells.len() {
            self.cells[self.column] = (character, self.style);
        } else {
            self.cells.push((character, self.style));
        }
        self.column += 1;
        if self.cells.len() >= MAX_LINE_CELLS {
            self.newline();
        }
    }

    fn newline(&mut self) {
        self.finished.push(render(&self.cells));
        self.cells.clear();
        self.column = 0;
    }
}

/// Builds the string and its style runs together, recording each run's byte offset as the
/// string grows. See [`Run`] for why the offsets are observed rather than computed.
fn render(cells: &[(char, Style)]) -> Line {
    let mut text = String::with_capacity(cells.len());
    let mut runs: Vec<Run> = Vec::new();
    for (character, style) in cells {
        let start = text.len();
        text.push(*character);
        match runs.last_mut() {
            Some(run) if run.style == *style => run.len += text.len() - start,
            _ => runs.push(Run { start, len: text.len() - start, style: *style }),
        }
    }
    Line { text, runs }
}

// ---------------------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------------------

/// A CLI agent running in a pseudo-terminal.
pub struct PtyTransport {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Cloned out of the child before it is handed to the reader thread. `ChildKiller` exists
    /// for exactly this: the reader thread blocks in `wait()` holding the child, so a
    /// `shutdown` that had to take the child back would deadlock against it.
    killer: Box<dyn ChildKiller + Send + Sync>,
    alive: Arc<AtomicBool>,
    exit: Arc<Mutex<Option<ExitStatus>>>,
    scrollback: Arc<Mutex<VecDeque<Line>>>,
    partial: Arc<Mutex<Line>>,
    events: Sender<TranscriptEvent>,
    reader: Option<JoinHandle<()>>,
    shut_down: bool,
}

impl std::fmt::Debug for PtyTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtyTransport")
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .field("shut_down", &self.shut_down)
            .finish_non_exhaustive()
    }
}

impl PtyTransport {
    /// Spawns the command in a pseudo-terminal.
    ///
    /// The binary is probed first, so a machine without it answers *"`claude` is not
    /// installed"* rather than a spawn error that names a syscall.
    pub fn start(spec: &LaunchSpec, events: Sender<TranscriptEvent>) -> Result<Self> {
        let command = spec.resolved_command().filter(|name| !name.is_empty()).ok_or_else(|| {
            AgentError::Refused(format!(
                "{} has no command to run in a terminal — choose one on the node",
                spec.provider.provider.label()
            ))
        })?;
        let program = probe_command(command)?;

        let (columns, rows) = spec.terminal.unwrap_or(DEFAULT_SIZE);
        let command_name = command.to_owned();
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize { rows, cols: columns, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| transport_error(&error))?;

        let mut builder = CommandBuilder::new(&program);
        // ⚠ **Velm's own MCP server, on the same terms `claude_cli` registers it.** A PTY
        // session is the Maestri mechanic — a real CLI agent in a real terminal on the board —
        // and it was the one transport that registered nothing, so an agent running here had
        // no research tools and reached the board only by shelling out to the shim on its
        // `PATH`. The two guards are `claude_cli`'s and matter for the same reasons: no
        // configuration is handed over naming a binary that is not beside us, and the flag is
        // only passed to a program whose own `--help` admits to it, because an unknown option
        // is a non-zero exit rather than an ignored argument.
        let mut mcp_dropped = false;
        if let Some(config) = spec.mcp_config() {
            if super::claude_cli::supports_flag(&program, MCP_CONFIG_FLAG) {
                builder.arg(MCP_CONFIG_FLAG);
                builder.arg(config.to_string());
            } else {
                mcp_dropped = true;
            }
        }
        for argument in &spec.args {
            builder.arg(argument);
        }
        if let Some(cwd) = &spec.cwd {
            builder.cwd(cwd);
        }
        for (key, value) in &spec.env {
            builder.env(key, value);
        }
        if !spec.system_context.is_empty() {
            builder.env(CONTEXT_ENV, &spec.system_context);
        }

        if mcp_dropped {
            let _ = events.send(TranscriptEvent::Error {
                message: format!(
                    "{command_name} did not accept `{MCP_CONFIG_FLAG}`, so Velm's tools are \
                     not registered with it. The board verbs still work through the \
                     `velm-agent-cli` command on its PATH; web research does not."
                ),
            });
        }

        let child = pair.slave.spawn_command(builder).map_err(|error| transport_error(&error))?;
        // The slave end must go, or the master never sees EOF when the child exits and the
        // reader thread blocks for the life of the process.
        drop(pair.slave);

        let killer = child.clone_killer();
        let output = pair.master.try_clone_reader().map_err(|error| transport_error(&error))?;
        let writer = pair.master.take_writer().map_err(|error| transport_error(&error))?;

        let alive = Arc::new(AtomicBool::new(true));
        let exit = Arc::new(Mutex::new(None));
        let scrollback = Arc::new(Mutex::new(VecDeque::with_capacity(64)));
        let partial = Arc::new(Mutex::new(Line::default()));

        let reader = std::thread::Builder::new()
            .name("velm-pty".into())
            .spawn({
                let alive = Arc::clone(&alive);
                let exit = Arc::clone(&exit);
                let scrollback = Arc::clone(&scrollback);
                let partial = Arc::clone(&partial);
                let events = events.clone();
                move || pump(output, child, &events, &alive, &exit, &scrollback, &partial)
            })
            .map_err(AgentError::Io)?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            master: Arc::new(Mutex::new(pair.master)),
            killer,
            alive,
            exit,
            scrollback,
            partial,
            events,
            reader: Some(reader),
            shut_down: false,
        })
    }

    /// The last [`MAX_SCROLLBACK`] completed lines, styled, for the painter.
    pub fn scrollback(&self) -> Vec<Line> {
        lock(&self.scrollback).iter().cloned().collect()
    }

    /// The line being written right now, which is not in the event stream yet.
    ///
    /// A prompt (`$ `) or a progress bar never ends in a newline, so it is never a completed
    /// line and never a `Terminal` event — the painter reads it here. The transcript on disk
    /// deliberately holds completed lines only: a partial line is, by definition, not
    /// finished, and appending every intermediate state of a spinner would be a megabyte a
    /// minute of history nobody wants.
    pub fn partial_line(&self) -> Line {
        lock(&self.partial).clone()
    }

    /// How the child exited, once it has.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        lock(&self.exit).clone()
    }

    /// Tells the child the window changed size.
    pub fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        lock(&self.master)
            .resize(PtySize { rows, cols: columns, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| transport_error(&error))
    }

    fn write_all(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = lock(&self.writer);
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|error| AgentError::Transport { transport: "pty", message: error.to_string() })
    }
}

impl AgentTransport for PtyTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Pty
    }

    /// Types the prompt into the terminal and **ends the turn immediately**.
    ///
    /// ⚠ This is the one place the PTY's event stream differs in shape from the other two,
    /// and it is forced rather than chosen. A terminal has no notion of a turn: output keeps
    /// arriving whether or not the agent considers itself finished, and nothing in the byte
    /// stream marks the end of an answer. The only way to detect "it went quiet" is to read
    /// the clock — which this crate is forbidden to do (`lib.rs`, and it is what makes every
    /// schedule testable as arithmetic).
    ///
    /// So the turn covers the *delivery*, not the answer. The alternative measured worse in
    /// every direction: a turn that never ends leaves the session `Running` forever, and a
    /// session that is permanently running queues every later prompt behind a turn that will
    /// not finish — the agent stops accepting input entirely.
    fn send_prompt(&mut self, turn: TurnId, prompt: &str) -> Result<()> {
        let _ = self.events.send(TranscriptEvent::TurnStarted {
            turn,
            prompt: prompt.to_owned(),
        });
        // A terminal's Enter is a carriage return, not a newline; a `\n` alone leaves many
        // line editors waiting for the rest of the line.
        let mut line = prompt.to_owned();
        line.push('\r');
        let sent = self.write_all(line.as_bytes());
        let outcome = match &sent {
            Ok(()) => TurnOutcome::Completed,
            Err(error) => TurnOutcome::Failed { message: error.to_string() },
        };
        let _ = self.events.send(TranscriptEvent::TurnEnded { turn, outcome });
        sent
    }

    /// Sends `Ctrl-C`, which is what a person would press.
    ///
    /// No `TurnEnded` follows: a PTY turn ended when the prompt was delivered (see
    /// [`PtyTransport::send_prompt`]), so there is nothing outstanding to close, and emitting
    /// a second `TurnEnded` for the same id would make the transcript unreplayable.
    fn cancel(&mut self) -> Result<()> {
        self.write_all(&[0x03])
    }

    fn write_input(&mut self, text: &str) -> Result<()> {
        self.write_all(text.as_bytes())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn shutdown(&mut self) -> Result<()> {
        if self.shut_down {
            return Ok(());
        }
        self.shut_down = true;
        // Kill first, join second: the reader is blocked on a read that only the child's
        // death unblocks, so joining first would hang for as long as the agent lives.
        let killed = self.killer.kill();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.alive.store(false, Ordering::Relaxed);
        killed.map_err(|error| AgentError::Transport {
            transport: "pty",
            message: error.to_string(),
        })
    }
}

impl Drop for PtyTransport {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The reader thread: bytes in, `Terminal` events out, and the child's fate at the end.
fn pump(
    mut output: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    events: &Sender<TranscriptEvent>,
    alive: &AtomicBool,
    exit: &Mutex<Option<ExitStatus>>,
    scrollback: &Mutex<VecDeque<Line>>,
    partial: &Mutex<Line>,
) {
    let mut terminal = Ansi::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        // A closed pty answers `Ok(0)` on some platforms and `EIO` on others — macOS raises
        // it the moment the last slave goes — so both are end of stream rather than a fault
        // worth reporting to the user.
        let read = match output.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        terminal.feed(&buffer[..read]);
        let lines = terminal.take_lines();
        if !lines.is_empty() {
            retain(scrollback, &lines);
            // One event per read rather than per line: a build log is thousands of lines a
            // second, and an event each would flood the channel the app drains per frame.
            let text =
                lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n");
            if events.send(TranscriptEvent::Terminal { text }).is_err() {
                break;
            }
        }
        *lock(partial) = terminal.current();
    }

    terminal.flush();
    let last = terminal.take_lines();
    if !last.is_empty() {
        retain(scrollback, &last);
        let text = last.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n");
        let _ = events.send(TranscriptEvent::Terminal { text });
    }
    *lock(partial) = Line::default();

    alive.store(false, Ordering::Relaxed);
    if let Ok(status) = child.wait() {
        let failed = !status.success();
        let described = status.to_string();
        *lock(exit) = Some(status);
        if failed {
            // An `Error`, not a `TurnEnded`: the process dying is not a turn's own failure,
            // and it can happen with no turn in flight at all.
            let _ = events.send(TranscriptEvent::Error {
                message: format!("the agent's terminal session ended: {described}"),
            });
        }
    }
}

fn retain(scrollback: &Mutex<VecDeque<Line>>, lines: &[Line]) {
    let mut back = lock(scrollback);
    for line in lines {
        if back.len() >= MAX_SCROLLBACK {
            back.pop_front();
        }
        back.push_back(line.clone());
    }
}

/// A poisoned lock here means a worker panicked mid-update. The data is a terminal buffer —
/// recovering it and carrying on shows slightly mangled output, where propagating the panic
/// would take the whole application down with it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn transport_error(error: &anyhow::Error) -> AgentError {
    AgentError::Transport { transport: "pty", message: error.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&[u8]]) -> Vec<String> {
        let mut terminal = Ansi::new();
        for chunk in chunks {
            terminal.feed(chunk);
        }
        terminal.flush();
        terminal.take_lines().into_iter().map(|line| line.text).collect()
    }

    /// Colour is the commonest thing in a CLI agent's output and must leave **no** residue.
    /// Asserting on the text is what catches a parser that consumed the `\x1b[` and then let
    /// the `31m` through as characters — the failure mode a naive `strip` has.
    #[test]
    fn colour_is_removed_from_the_text_and_kept_in_the_runs() {
        assert_eq!(feed(&[b"\x1b[31mred\x1b[0m plain"]), vec!["red plain"]);
        assert_eq!(feed(&[b"\x1b[1;38;5;208mbright\x1b[m"]), vec!["bright"]);
        // A private-mode set (hide cursor) and a truecolor run must both vanish whole.
        assert_eq!(feed(&[b"\x1b[?25l\x1b[38;2;10;20;30mx\x1b[0my"]), vec!["xy"]);

        let mut terminal = Ansi::new();
        terminal.feed(b"\x1b[1mbold\x1b[0mplain");
        let line = terminal.current();
        assert_eq!(line.text, "boldplain");
        assert_eq!(line.runs.len(), 2, "two styles must be two runs: {:?}", line.runs);
        assert!(line.runs[0].style.bold);
        assert!(!line.runs[1].style.bold);
        // The offsets must slice the string, which is the whole reason they are bytes
        // recorded as it was built.
        let bold = &line.runs[0];
        assert_eq!(&line.text[bold.start..bold.start + bold.len], "bold");
    }

    /// A carriage return overwrites in place. Every progress bar depends on it, and reading
    /// it as a newline is what turns one spinner into a thousand lines of transcript.
    #[test]
    fn a_carriage_return_overwrites_rather_than_starting_a_line() {
        assert_eq!(feed(&[b"abcdef\rXY"]), vec!["XYcdef"]);
        assert_eq!(feed(&[b"100%\r\x1b[Kdone"]), vec!["done"]);
        // Erase-to-start blanks up to and including the cursor rather than removing cells,
        // so what follows still sits where it did.
        assert_eq!(feed(&[b"abcdef\r\x1b[2C\x1b[1K"]), vec!["   def"]);
        // Erase-whole-line takes the lot. The `\r` first is how every CLI writes it — the
        // cursor does not move on its own, so without it the next text lands at column 5.
        assert_eq!(feed(&[b"noise\r\x1b[2Kkept"]), vec!["kept"]);
    }

    /// A read boundary lands wherever the kernel says. **This is the assertion that cannot
    /// pass on a per-chunk decoder**: `from_utf8_lossy` over each half yields `U+FFFD` twice
    /// and the emoji is gone, while the ANSI state machine equally must survive a sequence
    /// split down the middle.
    #[test]
    fn a_character_and_an_escape_split_across_two_reads_both_survive() {
        let emoji = "🙂".as_bytes();
        assert_eq!(emoji.len(), 4);
        assert_eq!(feed(&[&emoji[..2], &emoji[2..]]), vec!["🙂"]);

        let cjk = "日本語".as_bytes();
        assert_eq!(feed(&[&cjk[..4], &cjk[4..]]), vec!["日本語"]);

        // The escape sequence itself, cut in three.
        assert_eq!(feed(&[b"a\x1b", b"[3", b"1mb"]), vec!["ab"]);

        // Genuinely invalid bytes degrade to a replacement character rather than being
        // dropped or panicking — this is arbitrary process output, the least controlled
        // string in the application.
        assert_eq!(feed(&[b"a\xffb"]), vec!["a\u{fffd}b"]);
    }

    /// The memory bound, from the line's side. A process that never writes a newline must
    /// not be able to grow one line without limit — and the break must not lose bytes.
    #[test]
    fn a_line_that_never_ends_is_broken_rather_than_grown_without_limit() {
        let runaway = "x".repeat(MAX_LINE_CELLS * 2 + 5);
        let lines = feed(&[runaway.as_bytes()]);
        assert_eq!(lines.len(), 3, "the runaway line was not broken");
        assert_eq!(lines[0].chars().count(), MAX_LINE_CELLS);
        assert_eq!(
            lines.iter().map(|line| line.chars().count()).sum::<usize>(),
            runaway.chars().count(),
            "the break lost characters"
        );
    }

    /// **Ten bytes must not be able to ask for 16GB.** `\x1b[1000000000G` sets the cursor
    /// column from a number on the wire, and `put` pads with spaces up to that column
    /// *before* it consults [`MAX_LINE_CELLS`] — so an unclamped column is a billion
    /// sixteen-byte cells pushed one at a time, and `panic = "abort"` in the release profile
    /// means there is no catching the allocation failure.
    ///
    /// The existing runaway-line test cannot see this: it only feeds characters the child
    /// actually printed, and those go through `put` one per byte, where the cap does hold.
    ///
    /// A/B: with the `.min(MAX_LINE_CELLS - 1)` removed this does not fail, it hangs the test
    /// binary and then dies — which is precisely the report.
    #[test]
    fn a_cursor_column_from_the_wire_cannot_grow_the_line_past_its_cap() {
        let lines = feed(&[b"\x1b[1000000000Gx"]);
        assert_eq!(lines.len(), 1, "the clamped column should still be one line: {lines:?}");
        assert_eq!(
            lines[0].chars().count(),
            MAX_LINE_CELLS,
            "the padded line was not bounded by the line cap"
        );
        assert!(lines[0].ends_with('x'), "the character that followed the move was lost");

        // Cursor-forward is the same hole by addition, and it overflows before it clamps —
        // `usize::MAX` here wraps to a *small* column, which reads as working output.
        let lines = feed(&[b"a\x1b[18446744073709551615Cb"]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), MAX_LINE_CELLS);
        assert!(lines[0].starts_with('a') && lines[0].ends_with('b'));

        // Repeating the move must not accumulate either: each one lands at the same cap.
        let lines = feed(&[b"\x1b[900000000G\x1b[900000000G\x1b[900000000Gz"]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), MAX_LINE_CELLS);
    }

    /// **A tab at the end of a full line must end, and the clamp above is what put it there.**
    ///
    /// The tab arm works its stop out once and then pads towards it, and `put` breaks the line
    /// at [`MAX_LINE_CELLS`] — putting the column back to **zero**, below a stop it can now
    /// never reach. `feed` then never returns: a full 4,096-cell line goes into `finished`
    /// every 4,096 turns, for ever, and [`MAX_SCROLLBACK`] never prunes because it is
    /// downstream of the call that does not come back.
    ///
    /// The column clamp is what made it deterministic rather than lucky: `MAX_LINE_CELLS - 1`
    /// is *inside* the divergent window, so **every** `\x1b[NG` with N ≥ 4,089 followed by a
    /// tab lands in it. The second case needs no escape sequence at all.
    ///
    /// The existing clamp test cannot see this and neither could the runaway-line one: both
    /// feed a **printable** character after the move, which is the case that works — one `put`
    /// with no loop around it.
    ///
    /// A/B, and it is the same shape as the clamp test above: against the unfixed arm this
    /// does not fail, it hangs the test binary while its memory climbs.
    #[test]
    fn a_tab_at_the_end_of_a_full_line_stops_at_the_break_instead_of_looping_for_ever() {
        // The column comes off the wire, is clamped to 4,095, and the tab's stop is 4,096.
        let lines = feed(&[b"\x1b[4096G\t"]);
        assert_eq!(lines.len(), 1, "the padded line was not broken exactly once: {}", lines.len());
        assert_eq!(
            lines[0].chars().count(),
            MAX_LINE_CELLS,
            "the tab padded past the line cap"
        );

        // Reachable with no escape sequence: 4,090 printed characters put the column six
        // short of the break, and the next tab stop is past it.
        let mut typed = "x".repeat(MAX_LINE_CELLS - 6);
        typed.push('\t');
        let lines = feed(&[typed.as_bytes()]);
        assert_eq!(lines.len(), 1, "a tab after 4,090 characters did not end the line once");
        assert_eq!(lines[0].chars().count(), MAX_LINE_CELLS);
        assert!(lines[0].starts_with("xxxx"), "the characters before the tab were lost");

        // And the ordinary case is unchanged: a tab still advances to the next multiple of 8.
        assert_eq!(feed(&[b"a\tb"]), vec![format!("a{}b", " ".repeat(7))]);
    }

    /// The third memory bound. A CSI sequence ends at its *final* byte and nothing makes a
    /// child send one, so `\x1b[` followed by an endless run of digits grows the parameter
    /// buffer for as long as the process lives.
    ///
    /// The assertion is about **both** halves: bounded, and still not treating an ordinary
    /// byte as the sequence's terminator — dropping the overflow rather than ending the
    /// sequence is what stops noise from executing a command it happens to spell.
    #[test]
    fn an_unterminated_escape_sequence_does_not_grow_without_limit() {
        let mut terminal = Ansi::new();
        terminal.feed(b"\x1b[");
        for _ in 0..1000 {
            terminal.feed(b"1;2;3;4;5;6;7;8;9;0");
        }
        assert_eq!(
            terminal.parameters.len(),
            MAX_CSI_PARAMETER_BYTES,
            "the parameter buffer grew past its cap"
        );
        assert_eq!(terminal.state, State::Csi, "the sequence ended on a parameter byte");
        assert_eq!(terminal.current().text, "", "parameter bytes reached the text");

        // The sequence still ends where it is supposed to, and what follows is ordinary text.
        terminal.feed(b"mafter");
        assert_eq!(terminal.state, State::Ground);
        assert_eq!(terminal.current().text, "after");
    }

    /// Tabs, backspace and the bell all reach the text if nothing handles them, and a bare
    /// `\x07` in a transcript is a black box on the canvas.
    #[test]
    fn control_characters_do_not_reach_the_text() {
        assert_eq!(feed(&[b"a\tb"]), vec![format!("a{}b", " ".repeat(7))]);
        assert_eq!(feed(&[b"ab\x08c"]), vec!["ac"]);
        assert_eq!(feed(&[b"ding\x07"]), vec!["ding"]);
        // A window title is an OSC string and every byte of it, including the text, is ours
        // to drop — it is not output, it is a request to the terminal emulator.
        assert_eq!(feed(&[b"\x1b]0;a title\x07after"]), vec!["after"]);
        assert_eq!(feed(&[b"\x1b]0;another\x1b\\after"]), vec!["after"]);
    }

    /// A repaint discards the partial line rather than emitting a copy of the screen. The
    /// assertion is about what *doesn't* accumulate: without it, a TUI agent writes its whole
    /// screen into the transcript on every frame.
    #[test]
    fn homing_the_cursor_discards_the_partial_line_instead_of_emitting_it() {
        assert_eq!(feed(&[b"stale\x1b[Hfresh"]), vec!["fresh"]);
        assert_eq!(feed(&[b"stale\x1b[2Jfresh"]), vec!["fresh"]);
        // A completed line before the repaint is already out and stays out.
        assert_eq!(feed(&[b"kept\nstale\x1b[Hfresh"]), vec!["kept", "fresh"]);
    }

    #[test]
    fn lines_are_separated_and_a_partial_line_is_not_one_yet() {
        let mut terminal = Ansi::new();
        terminal.feed(b"one\ntwo\nthree");
        let done: Vec<String> =
            terminal.take_lines().into_iter().map(|line| line.text).collect();
        assert_eq!(done, vec!["one", "two"]);
        assert_eq!(terminal.current().text, "three", "the partial line leaked into the stream");

        terminal.flush();
        let last: Vec<String> =
            terminal.take_lines().into_iter().map(|line| line.text).collect();
        assert_eq!(last, vec!["three"], "end of stream lost the last line");
    }

    /// The other half of the memory bound. Two thousand lines is the cap; the two thousand
    /// and first must push the oldest out rather than growing the buffer.
    #[test]
    fn scrollback_is_bounded_and_keeps_the_newest() {
        let scrollback = Mutex::new(VecDeque::new());
        let lines: Vec<Line> = (0..MAX_SCROLLBACK + 10)
            .map(|index| Line { text: format!("{index}"), runs: Vec::new() })
            .collect();
        retain(&scrollback, &lines);

        let held = scrollback.lock().unwrap();
        assert_eq!(held.len(), MAX_SCROLLBACK);
        assert_eq!(held.front().unwrap().text, "10", "the wrong end was dropped");
        assert_eq!(held.back().unwrap().text, format!("{}", MAX_SCROLLBACK + 9));
    }
}
