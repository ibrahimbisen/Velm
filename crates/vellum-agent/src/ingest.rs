//! Turning a file, a page or a video into text an agent can be given.
//!
//! Feature 18: *"feed in any file type as context"*. What reaches an agent is always a
//! string; this module is the part that decides how a `.pdf`, a `.docx`, a YouTube link or a
//! voice memo becomes one, and — just as often — says plainly that it cannot.
//!
//! # Nothing here is ever silently empty
//!
//! [`ingest`] cannot fail. It returns an [`Ingested`] whose [`Outcome`] always carries a
//! sentence naming what happened and what would fix it: *"a `.docx` needs `textutil`, which
//! this machine has not got"*, not an empty string and a green tick. That is the repo's
//! standing rule that nothing is inert, applied to the one place where a quiet failure is
//! indistinguishable from a boring file — an agent given zero characters of context does not
//! complain, it just answers badly.
//!
//! # Shelling out is a real implementation, not a fallback
//!
//! This crate's dependency list is `serde`, `serde_json`, `thiserror`, `anyhow`, `ureq` and
//! `portable-pty`. There is no flate decoder in it and no ZIP reader, so the compressed
//! halves of PDF and `.docx` cannot be read here. Rather than pretend, [`Tools`] probes
//! `PATH` for `pdftotext` and macOS's `textutil` and uses them when they are there — the
//! user's own machine already carries the right converter surprisingly often, `textutil`
//! ships with macOS, and a tool that exists is a better answer than a dependency that has to
//! be argued for. When neither is present the outcome names the missing binary.
//!
//! **The probe is injectable** ([`Tools::none`]) so the degraded message is an ordinary unit
//! test rather than something only reproducible on a machine that happens to lack the tool.
//!
//! # What is tested and what is not
//!
//! Every parser here is a pure function over a `&str` or a `&[u8]` and is tested on
//! literals. The three functions that touch the network are **not** tested: `cargo test` must
//! stay offline and deterministic, which is why `vellum-link`'s only honest network check is
//! an example rather than a test. Treat a *"the web ingest returns nothing"* report as
//! unverified-by-design and start there.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::model::ContextSource;

/// How much of a local file is read.
///
/// A ceiling, not an expectation. An agent's context window is measured in hundreds of
/// thousands of characters, so a file larger than this cannot be given whole in any case,
/// and reading a two-gigabyte log into memory to throw most of it away is how a node placed
/// on a board takes the machine down. The truncation is reported rather than hidden.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

/// How much of a web page is read.
///
/// `vellum-link` stops at `</head>` and caps at 1MB, because a card only needs the meta
/// tags. This needs the **body**, so neither trick applies. 2MB is comfortably past an
/// ordinary article and still refuses a response that never ends.
pub const WEB_MAX_BYTES: usize = 2 * 1024 * 1024;

/// How much of a YouTube watch page is read.
///
/// **Measured, 2026-08-13, on a real watch page**: the document is 1,338,302 bytes and
/// `"captionTracks"` sits at byte 724,908 — three quarters of a megabyte past `</head>`,
/// which is why the head-only trick that serves a link card is useless here. 3MB is that
/// measurement with headroom for a page with more comments in its initial payload; it is one
/// page's number, not a distribution, and it is a cap rather than a target.
pub const YOUTUBE_MAX_BYTES: usize = 3 * 1024 * 1024;

/// Named as ourselves. The same reasoning `vellum-link` records: plenty of sites serve less
/// to an unrecognised agent, and impersonating a browser is a lie that also goes stale.
const USER_AGENT: &str = concat!("Velm/", env!("CARGO_PKG_VERSION"), " (agent context)");

/// What the ingester decided a source is.
///
/// The tags match [`ContextSource::kind`]'s documented vocabulary exactly, because that
/// string is what is written into a board file and read back by a later build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Plain text, markdown, source code, JSON, CSV — anything that is already characters.
    Text,
    Pdf,
    /// A word processor document: `.docx`, `.doc`, `.rtf`, `.odt`.
    Docx,
    Audio,
    Video,
    Image,
    /// A YouTube watch link, which is a web page with a title and — in principle — a
    /// caption track. See [`youtube`].
    YouTube,
    /// Any other http(s) address.
    Web,
    /// A file whose extension says nothing. Resolved by sniffing the bytes, not refused —
    /// `Makefile`, `Dockerfile` and `.env` all land here and all of them are text.
    Unknown,
}

impl Kind {
    /// The tag written into [`ContextSource::kind`]. Stable: it goes into board files.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Text | Self::Unknown => "text",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Image => "image",
            Self::YouTube => "youtube",
            Self::Web => "web",
        }
    }

    /// Whether this kind is handed to the transcription path rather than read here.
    pub const fn is_media(self) -> bool {
        matches!(self, Self::Audio | Self::Video)
    }
}

/// Which external converters this machine has.
///
/// Probed from `PATH` by looking for the file, **not** by running it: a probe that executes
/// a binary is a probe that can hang, and `pdftotext -v` exits non-zero on some builds, so
/// running it would report the tool as missing when it is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tools {
    /// poppler's `pdftotext`, the honest answer for a compressed PDF.
    pub pdftotext: bool,
    /// macOS's own converter. Reads `.docx`, `.doc`, `.rtf`, `.odt` and `.html`, and it is
    /// on every Mac, which makes it the highest-value tool in this list by a distance.
    pub textutil: bool,
}

impl Tools {
    /// What is actually installed.
    pub fn probe() -> Self {
        Self { pdftotext: on_path("pdftotext"), textutil: on_path("textutil") }
    }

    /// Nothing installed. The constructor that makes the degraded path testable — without
    /// it, *"say so plainly when the tool is missing"* could only be checked on a machine
    /// that happened to be missing it, which is not a test.
    pub const fn none() -> Self {
        Self { pdftotext: false, textutil: false }
    }
}

/// The limits an ingestion runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub max_file_bytes: usize,
    pub timeout: Duration,
    pub tools: Tools,
}

impl Default for Options {
    /// Note that this **probes `PATH`**, which is a handful of `stat` calls. Cheap, and done
    /// once per ingestion rather than once per frame — nothing here runs on the frame loop.
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            timeout: Duration::from_secs(15),
            tools: Tools::probe(),
        }
    }
}

/// A media file that this module deliberately extracts nothing from.
///
/// **The hand-off shape, and the whole of this module's involvement with audio.** Velm's
/// speech-to-text lives in `voice.rs`; decoding an audio file here would put a second
/// transcription path in the application, and the two would disagree about the same file.
///
/// The contract: ingestion produces the [`ContextSource`] — kind `audio` or `video`, labelled
/// with the file's name — with [`ContextSource::extract`] left `None`. Whoever owns
/// transcription reads this hand-off, produces the text, writes it into the sidecar cache and
/// fills `extract` with its hash. Until that happens the node shows a source that is attached
/// and not yet transcribed, which is the true state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaHandoff {
    pub path: PathBuf,
    /// [`Kind::Audio`] or [`Kind::Video`].
    pub kind: Kind,
}

/// What happened, in a form that always has something to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// All of it was extracted. The count is here because *"attached — 0 characters"* is a
    /// failure wearing a success's clothes, and the node can say which one it got.
    Extracted { chars: usize },
    /// Some of it was extracted, and knowably not all — a file past the cap, a video whose
    /// captions were refused. The message says what is missing.
    ///
    /// Distinct from [`Outcome::Extracted`] rather than a `truncated: bool` on it, because
    /// the *reasons* differ and a boolean forces one sentence to cover all of them. A video
    /// with no captions was reported as "the rest was too long to attach", which is a lie
    /// about a file that was read whole.
    Partial { chars: usize, message: String },
    /// Recognised, and the text is behind a converter this machine has not got.
    NeedsTool { tool: &'static str, message: String },
    /// Recognised, and handed to the transcription path rather than read here.
    Transcribe(MediaHandoff),
    /// Recognised, and not a thing text comes out of.
    Unsupported { message: String },
    /// It could not be read or reached at all.
    Failed { message: String },
}

impl Outcome {
    /// The sentence shown on the node. Never empty, for any variant.
    pub fn message(&self) -> String {
        match self {
            Self::Extracted { chars } => format!("Read {chars} characters"),
            Self::Partial { message, .. }
            | Self::NeedsTool { message, .. }
            | Self::Unsupported { message }
            | Self::Failed { message } => message.clone(),
            Self::Transcribe(handoff) => format!(
                "Attached as {} and queued for transcription",
                if handoff.kind == Kind::Video { "video" } else { "audio" }
            ),
        }
    }

    /// Whether an agent got any characters out of this.
    pub const fn is_text(&self) -> bool {
        matches!(self, Self::Extracted { .. } | Self::Partial { .. })
    }
}

/// One ingested source: what to record on the node, and what to give the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    /// What goes into [`crate::AgentModel::context`].
    ///
    /// [`ContextSource::extract`] is always `None` here. That field is a BLAKE3 hash into the
    /// sidecar cache and **this crate has no `blake3`** — deliberately, since it depends on
    /// nothing in the workspace. The caller writes the text to the cache, hashes it with the
    /// same blob store every pasted screenshot goes through, and fills the field in.
    pub source: ContextSource,
    /// The text an agent can be given. Empty unless [`Outcome::is_text`].
    pub text: String,
    pub outcome: Outcome,
}

impl Ingested {
    fn new(
        source: &str,
        kind: Kind,
        label: impl Into<String>,
        text: String,
        outcome: Outcome,
    ) -> Self {
        Self {
            source: ContextSource {
                source: source.to_owned(),
                kind: kind.tag().to_owned(),
                extract: None,
                label: label.into(),
            },
            text,
            outcome,
        }
    }

    fn failed(source: &str, kind: Kind, label: impl Into<String>, message: String) -> Self {
        Self::new(source, kind, label, String::new(), Outcome::Failed { message })
    }
}

/// What a path or a URL is, from its spelling alone.
///
/// Extension-based and case-insensitive. Cheap on purpose: this is called to decide whether
/// to offer an attachment at all, before anything has been opened.
pub fn classify(source: &str) -> Kind {
    let trimmed = source.trim();
    let lower = trimmed.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return if is_youtube(&lower) { Kind::YouTube } else { Kind::Web };
    }

    let extension = Path::new(trimmed)
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "txt" | "md" | "markdown" | "rst" | "org" | "log" | "csv" | "tsv" | "json" | "jsonl"
        | "toml" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "conf" | "env" | "rs" | "py"
        | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "rb" | "java" | "kt" | "swift"
        | "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "php" | "sh" | "bash" | "zsh" | "fish"
        | "sql" | "css" | "scss" | "html" | "htm" | "vue" | "svelte" | "lua" | "pl" | "r"
        | "tex" | "gradle" | "cmake" | "dockerfile" | "makefile" | "lock" | "patch" | "diff" => {
            Kind::Text
        }
        "pdf" => Kind::Pdf,
        "docx" | "doc" | "rtf" | "odt" | "pages" => Kind::Docx,
        "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" | "oga" | "opus" | "aiff" | "aif"
        | "wma" => Kind::Audio,
        "mp4" | "mov" | "m4v" | "mkv" | "avi" | "webm" | "wmv" | "mpg" | "mpeg" => Kind::Video,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "heic" | "svg"
        | "ico" => Kind::Image,
        _ => Kind::Unknown,
    }
}

/// Ingests a source with the default limits and a real `PATH` probe.
pub fn ingest(source: &str) -> Ingested {
    ingest_with(source, &Options::default())
}

/// [`ingest`] with the limits and the tool set spelled out.
///
/// Infallible by design. Every failure is a message on the node, and a `Result` here would be
/// unwrapped into exactly this shape at the one call site — the same argument `lib.rs` makes
/// for having one error type rather than six.
pub fn ingest_with(source: &str, options: &Options) -> Ingested {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        let why = "There is nothing to attach.".to_owned();
        return Ingested::failed(source, Kind::Unknown, "nothing", why);
    }

    match classify(trimmed) {
        Kind::YouTube => youtube(trimmed, options),
        Kind::Web => web(trimmed, options),
        kind => file(trimmed, kind, options),
    }
}

// ---------------------------------------------------------------------------------------
// Local files
// ---------------------------------------------------------------------------------------

fn file(source: &str, kind: Kind, options: &Options) -> Ingested {
    let path = Path::new(source);
    let label = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_owned());

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Ingested::failed(
                source,
                kind,
                label,
                format!("{source} could not be opened: {error}"),
            );
        }
    };
    if metadata.is_dir() {
        // Not a failure, and worth saying rather than refusing: a directory *is* supported,
        // by the node kind built for it.
        return Ingested::new(
            source,
            kind,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!(
                    "{source} is a folder. Place a file-tree node on it instead — that is \
                     what gives an agent a directory."
                ),
            },
        );
    }

    match kind {
        Kind::Audio | Kind::Video => Ingested::new(
            source,
            kind,
            label,
            String::new(),
            Outcome::Transcribe(MediaHandoff { path: path.to_path_buf(), kind }),
        ),
        Kind::Image => Ingested::new(
            source,
            kind,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!(
                    "{source} is a picture. An agent is given a picture as a picture rather \
                     than as text — paste it onto the board and connect it."
                ),
            },
        ),
        Kind::Pdf => pdf(source, &label, path, options),
        Kind::Docx => docx(source, &label, path, options),
        Kind::Text | Kind::Unknown => text_file(source, &label, path, kind, options),
        Kind::Web | Kind::YouTube => unreachable!("a URL never reaches the file path"),
    }
}

/// Reads a file as text, lossily.
///
/// **Lossy, never a panic.** A file the user attached is arbitrary bytes: a `.txt` saved in
/// Latin-1, a log with a truncated multi-byte character at the end, a binary somebody
/// mis-named. `from_utf8_lossy` substitutes U+FFFD and carries on, which is also what makes
/// the byte-level truncation above it safe — a cut through the middle of a character becomes
/// one replacement character rather than a panic.
fn text_file(source: &str, label: &str, path: &Path, kind: Kind, options: &Options) -> Ingested {
    // One byte past the cap, so "there was more" and "there was exactly this much" are
    // distinguishable. Reading exactly the cap and testing `len >= cap` reports a file whose
    // length happens to be the cap as truncated, which is a message that is simply untrue.
    let mut bytes = match read_capped(path, options.max_file_bytes.saturating_add(1)) {
        Ok(bytes) => bytes,
        Err(error) => {
            let why = format!("{source} could not be read: {error}");
            return Ingested::failed(source, kind, label, why);
        }
    };
    let truncated = bytes.len() > options.max_file_bytes;
    if truncated {
        bytes.truncate(options.max_file_bytes);
    }

    // An unknown extension is decided by its bytes rather than refused. A NUL byte is the
    // one reliable signal that a file is not text — no encoding of readable characters
    // contains one — and it is what `grep` and `git` both use.
    if kind == Kind::Unknown && bytes.contains(&0) {
        return Ingested::new(
            source,
            Kind::Unknown,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!(
                    "{source} does not look like text, and Velm has no reader for this kind \
                     of file. Convert it first, or attach it to the agent as a file path."
                ),
            },
        );
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();
    let chars = text.chars().count();
    let outcome = if truncated {
        Outcome::Partial {
            chars,
            message: format!(
                "Read the first {chars} characters of {label}; it is longer than Velm attaches."
            ),
        }
    } else {
        Outcome::Extracted { chars }
    };
    Ingested::new(source, Kind::Text, label, text, outcome)
}

fn read_capped(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    std::io::BufReader::new(file).take(cap as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------
// PDF
// ---------------------------------------------------------------------------------------

fn pdf(source: &str, label: &str, path: &Path, options: &Options) -> Ingested {
    // The external tool first when it is there: it reads every PDF, where the built-in
    // extractor reads the uncompressed minority. Preferring our own would mean answering
    // "this is compressed" on a machine that could have read it.
    //
    // A tool that is installed and *fails* is not a reason to give up: the `if let` falls
    // through to the built-in extractor, which may still get something out of the file.
    //
    // ⚠ The emptiness check is doing the same job for a *worse* case, and it is measured
    // rather than defensive: `pdftotext` exits **0 and prints nothing** for a PDF with no
    // page tree, which is exactly what a hand-written or machine-generated stub PDF is. So
    // "the tool succeeded" is not the same claim as "the tool got the text", and without
    // this line a file the built-in extractor reads perfectly well returns zero characters
    // on any machine that happens to have poppler installed.
    if options.tools.pdftotext
        && let Ok(raw) = run_tool("pdftotext", &["-q", "-layout"], path, true)
    {
        let text = normalise_text(&raw);
        if !text.is_empty() {
            let chars = text.chars().count();
            return Ingested::new(source, Kind::Pdf, label, text, Outcome::Extracted { chars });
        }
    }

    // Cap + 1 and a truncation flag, exactly as `text_file` does. A PDF past the cap loses
    // whole content streams — i.e. whole pages — and reporting that as a complete read is
    // the same untrue message, arriving by a quieter route: the text that *is* extracted
    // looks perfectly ordinary.
    let mut bytes = match read_capped(path, options.max_file_bytes.saturating_add(1)) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ingested::failed(source, Kind::Pdf, label, format!("{source}: {error}"));
        }
    };
    let cut = bytes.len() > options.max_file_bytes;
    if cut {
        bytes.truncate(options.max_file_bytes);
    }

    match pdf_text(&bytes) {
        PdfText::Text(text) => {
            let chars = text.chars().count();
            let outcome = if cut {
                Outcome::Partial {
                    chars,
                    message: format!(
                        "Read the first {} MB of {label}; later pages were not read.",
                        options.max_file_bytes / (1024 * 1024)
                    ),
                }
            } else {
                Outcome::Extracted { chars }
            };
            Ingested::new(source, Kind::Pdf, label, text, outcome)
        }
        PdfText::Compressed { streams } => Ingested::new(
            source,
            Kind::Pdf,
            label,
            String::new(),
            Outcome::NeedsTool {
                tool: "pdftotext",
                message: format!(
                    "{label} keeps its text in {streams} compressed stream(s), and Velm has \
                     no decompressor built in. Install `pdftotext` (it comes with poppler: \
                     `brew install poppler`) and attach the file again."
                ),
            },
        ),
        PdfText::NoText => Ingested::new(
            source,
            Kind::Pdf,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!(
                    "{label} has no text in it — it is most likely a scan. It needs optical \
                     character recognition, which Velm does not do."
                ),
            },
        ),
        PdfText::NotPdf => Ingested::new(
            source,
            Kind::Pdf,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!("{label} is named as a PDF and does not begin like one."),
            },
        ),
    }
}

/// What [`pdf_text`] could get out of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfText {
    Text(String),
    /// The content streams are `FlateDecode`d, and there is no flate decoder in this crate's
    /// dependency set. The count is reported so the message can be specific.
    Compressed { streams: usize },
    /// It parsed and showed no text at all — a scanned page, essentially.
    NoText,
    NotPdf,
}

/// A deliberately limited PDF text extractor.
///
/// **What it does**: finds `stream … endstream` objects whose dictionary carries no
/// `/Filter`, tokenises them as content streams, and collects the operands of the four
/// text-showing operators — `Tj`, `TJ`, `'` and `"`. Literal `(…)` strings with their escapes
/// and nesting, and hex `<…>` strings, are both decoded; a `Td`, `TD`, `T*` or `ET` becomes a
/// line break, which is the only positioning this pays attention to.
///
/// **What it does not do**, and this is most PDFs: decompress anything. Producers have
/// emitted `FlateDecode`d content streams by default for twenty years, so the common case
/// answers [`PdfText::Compressed`] and the message names `pdftotext`. It also does not
/// resolve font encodings or `/ToUnicode` maps — a document in a subset-encoded font will
/// come out as the wrong letters, and there is no way to notice that from inside here —
/// does not handle cross-reference streams, object streams, or encryption, and makes no
/// attempt at reading order across columns.
///
/// This is offered as the honest partial answer rather than as a PDF reader.
pub fn pdf_text(bytes: &[u8]) -> PdfText {
    // Some producers put junk before the header, so the marker is looked for rather than
    // required at byte zero.
    let head = &bytes[..bytes.len().min(1024)];
    if find_bytes(head, b"%PDF", 0).is_none() {
        return PdfText::NotPdf;
    }

    let mut compressed = 0usize;
    let mut collected = String::new();
    let mut index = 0usize;

    while let Some(at) = find_bytes(bytes, b"stream", index) {
        // `endstream` contains `stream`. Stepping over it here is cheaper and clearer than
        // a search that has to know about word boundaries.
        if at >= 3 && bytes.get(at - 3..at) == Some(b"end".as_slice()) {
            index = at + 6;
            continue;
        }

        let dictionary = bytes.get(rfind_bytes(&bytes[..at], b"<<").unwrap_or(0)..at).unwrap_or(&[]);
        // The keyword is followed by CRLF or LF, and the payload starts after it.
        let mut body_start = at + 6;
        if bytes.get(body_start) == Some(&b'\r') {
            body_start += 1;
        }
        if bytes.get(body_start) == Some(&b'\n') {
            body_start += 1;
        }
        let body_end = find_bytes(bytes, b"endstream", body_start).unwrap_or(bytes.len());
        let body = bytes.get(body_start..body_end).unwrap_or(&[]);

        if find_bytes(dictionary, b"/Filter", 0).is_some() {
            // An image stream is filtered too and is not a failure to read text from, so
            // only the compression filters are counted towards the "needs a tool" verdict.
            if find_bytes(dictionary, b"FlateDecode", 0).is_some()
                || find_bytes(dictionary, b"LZWDecode", 0).is_some()
            {
                compressed += 1;
            }
        } else {
            let text = pdf_content_text(body);
            if !text.trim().is_empty() {
                collected.push_str(&text);
                collected.push('\n');
            }
        }
        index = body_end + 9;
    }

    let text = normalise_text(&collected);
    if !text.is_empty() {
        PdfText::Text(text)
    } else if compressed > 0 {
        PdfText::Compressed { streams: compressed }
    } else {
        PdfText::NoText
    }
}

/// Pulls the shown text out of one uncompressed content stream.
fn pdf_content_text(stream: &[u8]) -> String {
    let mut out = String::new();
    let mut pending = String::new();
    let mut index = 0usize;

    while index < stream.len() {
        match stream[index] {
            b'(' => {
                let (text, next) = pdf_literal(stream, index + 1);
                pending.push_str(&text);
                index = next;
            }
            b'<' => {
                if stream.get(index + 1) == Some(&b'<') {
                    // A dictionary, not a string. Skipped over; its contents tokenise as
                    // names and numbers, which are ignored anyway.
                    index += 2;
                } else {
                    let (text, next) = pdf_hex(stream, index + 1);
                    pending.push_str(&text);
                    index = next;
                }
            }
            b'/' => {
                // A name. Skipped whole so that an operator-shaped name — `/Tj` in a
                // properties dictionary — cannot be mistaken for the operator.
                index += 1;
                while index < stream.len() && !is_pdf_delimiter(stream[index]) {
                    index += 1;
                }
            }
            byte if byte.is_ascii_alphabetic() || byte == b'\'' || byte == b'"' => {
                let start = index;
                while index < stream.len()
                    && (stream[index].is_ascii_alphanumeric()
                        || stream[index] == b'*'
                        || stream[index] == b'\''
                        || stream[index] == b'"')
                {
                    index += 1;
                }
                // Compared as text rather than as a byte-string pattern: an operator is ASCII
                // by construction, and `match slice { b"Tj" => … }` on a `&[u8]` is a newer
                // language feature than this codebase should depend on for two comparisons.
                let token = String::from_utf8_lossy(stream.get(start..index).unwrap_or(&[]));
                match token.as_ref() {
                    "Tj" | "TJ" | "'" | "\"" => {
                        out.push_str(&pending);
                        pending.clear();
                    }
                    "Td" | "TD" | "T*" | "ET" => {
                        out.push_str(&pending);
                        pending.clear();
                        out.push('\n');
                    }
                    _ => {}
                }
            }
            _ => index += 1,
        }
    }
    out.push_str(&pending);
    out
}

/// A `(…)` string, from just after the opening bracket. Returns the text and the index past
/// the closing bracket.
fn pdf_literal(stream: &[u8], from: usize) -> (String, usize) {
    let mut raw: Vec<u8> = Vec::new();
    let mut depth = 1usize;
    let mut index = from;
    while index < stream.len() {
        match stream[index] {
            b'\\' => {
                index += 1;
                // Destructured by value, so every arm below compares two `u8`s rather than
                // leaning on match ergonomics to see through the reference.
                let Some(&escape) = stream.get(index) else { break };
                match escape {
                    b'n' => raw.push(b'\n'),
                    b'r' => raw.push(b'\r'),
                    b't' => raw.push(b'\t'),
                    b'b' => raw.push(8),
                    b'f' => raw.push(12),
                    b'\n' => {}  // A line continuation inside a string.
                    b'\r' => {
                        if stream.get(index + 1) == Some(&b'\n') {
                            index += 1;
                        }
                    }
                    digit if digit.is_ascii_digit() => {
                        // Up to three octal digits.
                        let mut value = u32::from(digit - b'0');
                        for _ in 0..2 {
                            match stream.get(index + 1) {
                                Some(&next) if next.is_ascii_digit() && next < b'8' => {
                                    value = value * 8 + u32::from(next - b'0');
                                    index += 1;
                                }
                                _ => break,
                            }
                        }
                        raw.push((value & 0xFF) as u8);
                    }
                    other => raw.push(other),
                }
                index += 1;
            }
            b'(' => {
                depth += 1;
                raw.push(b'(');
                index += 1;
            }
            b')' => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    break;
                }
                raw.push(b')');
            }
            byte => {
                raw.push(byte);
                index += 1;
            }
        }
    }
    (pdf_string_to_text(&raw), index)
}

/// A `<…>` hex string, from just after the opening angle bracket.
fn pdf_hex(stream: &[u8], from: usize) -> (String, usize) {
    let mut digits: Vec<u8> = Vec::new();
    let mut index = from;
    while index < stream.len() && stream[index] != b'>' {
        if stream[index].is_ascii_hexdigit() {
            digits.push(stream[index]);
        }
        index += 1;
    }
    // An odd number of digits is padded with a trailing zero, which is what the spec says.
    if digits.len() % 2 == 1 {
        digits.push(b'0');
    }
    let raw: Vec<u8> = digits
        .chunks(2)
        .filter_map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect();
    (pdf_string_to_text(&raw), index + 1)
}

/// Decodes a PDF string's bytes to characters.
///
/// Two encodings, which are the two that occur: **UTF-16BE** when the string opens with the
/// byte-order mark `FE FF`, which is how any non-Latin text is written; and **WinAnsi**
/// otherwise, which agrees with Latin-1 everywhere except `0x80`–`0x9F`. That range is the
/// one people notice — it holds the curly quotes, the dashes and the ellipsis that a word
/// processor inserts automatically — so it is spelled out rather than approximated.
fn pdf_string_to_text(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        let units: Vec<u16> = raw
            .get(2..)
            .unwrap_or(&[])
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        // `decode_utf16` rather than `char::from_u32` per unit: a surrogate *pair* is two
        // units and one character, and decoding them separately yields two replacement
        // characters for every emoji and every rarer CJK ideograph.
        return char::decode_utf16(units).map(|unit| unit.unwrap_or('\u{FFFD}')).collect();
    }
    raw.iter().map(|byte| win_ansi(*byte)).collect()
}

/// PDF's own delimiter set, plus whitespace: what ends a name token.
const fn is_pdf_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// WinAnsiEncoding, which is Latin-1 with a different `0x80`–`0x9F` block.
fn win_ansi(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        other => char::from(other),
    }
}

// ---------------------------------------------------------------------------------------
// Word processor documents
// ---------------------------------------------------------------------------------------

/// `.docx` and friends, which is one shell-out or one clear refusal.
///
/// A `.docx` is a ZIP holding `word/document.xml`, and this crate has neither a ZIP reader
/// nor a decompressor — the workspace's `zip` crate is not a dependency here and cannot be
/// made one from inside this module. `textutil` ships with macOS and reads all four of these
/// formats, so on the platform Velm ships on first this is a complete implementation; on
/// Windows it is a named refusal until either `zip` is added to this crate or a converter is
/// found. Both are recorded rather than papered over.
fn docx(source: &str, label: &str, path: &Path, options: &Options) -> Ingested {
    if options.tools.textutil {
        match run_tool("textutil", &["-convert", "txt", "-stdout"], path, false) {
            Ok(raw) => {
                let text = normalise_text(&raw);
                let chars = text.chars().count();
                return Ingested::new(
                    source,
                    Kind::Docx,
                    label,
                    text,
                    Outcome::Extracted { chars },
                );
            }
            Err(error) => {
                return Ingested::failed(
                    source,
                    Kind::Docx,
                    label,
                    format!("`textutil` could not read {label}: {error}"),
                );
            }
        }
    }

    Ingested::new(
        source,
        Kind::Docx,
        label,
        String::new(),
        Outcome::NeedsTool {
            tool: "textutil",
            message: format!(
                "{label} is a word processor document, and Velm has no reader for one built \
                 in. On macOS `textutil` does this and ships with the system; elsewhere, save \
                 the document as `.txt` or `.md` and attach that."
            ),
        },
    )
}

// ---------------------------------------------------------------------------------------
// The web
// ---------------------------------------------------------------------------------------

fn web(source: &str, options: &Options) -> Ingested {
    let host = host_of(source).unwrap_or_else(|| source.to_owned());
    let (page, cut) = match get_capped(source, options, WEB_MAX_BYTES) {
        Ok(page) => page,
        Err(message) => return Ingested::failed(source, Kind::Web, host, message),
    };

    let title = html_title(&page);
    let label = title.clone().unwrap_or(host);
    let body = html_to_text(&page);
    if body.trim().is_empty() {
        return Ingested::new(
            source,
            Kind::Web,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!(
                    "{source} returned a page with no readable text in it. It is most likely \
                     drawn by JavaScript, which Velm does not run."
                ),
            },
        );
    }

    // The title is prepended because the extracted body very often does not contain it — it
    // lives in the `<head>` — and an agent given a wall of prose with no idea what page it
    // came from is being given a worse version of the same context.
    let text = match &title {
        Some(title) => format!("{title}\n{source}\n\n{body}"),
        None => format!("{source}\n\n{body}"),
    };
    let chars = text.chars().count();
    let outcome = if cut {
        Outcome::Partial {
            chars,
            message: format!(
                "Read the first {} KB of {source}; the page is longer than Velm fetches.",
                WEB_MAX_BYTES / 1024
            ),
        }
    } else {
        Outcome::Extracted { chars }
    };
    Ingested::new(source, Kind::Web, label, text, outcome)
}

/// A YouTube link: the title always, the captions when YouTube will serve them.
///
/// **Measured, and this is the part to believe rather than the hope**: on 2026-08-13 the
/// watch page yielded its title and its `captionTracks` array without difficulty, and the
/// `baseUrl` inside that array — fetched exactly as written, and again with `&fmt=srv3` —
/// answered **HTTP 200 with a zero-byte body**. YouTube now gates the timedtext endpoint
/// behind a session token that a plain client does not have. So the honest expectation is
/// **title-only**, and the caption path is kept because it costs one request, because the
/// parsing is pure and tested, and because the gate is a server-side decision that can move
/// back. It degrades to the title rather than failing, which is the whole point.
///
/// Scraping a page for this is fragile in the ordinary way — `captionTracks` is an
/// implementation detail of YouTube's own player payload, not an interface, and it will be
/// renamed one day with no notice. That is an argument for degrading well, not for not
/// doing it.
fn youtube(source: &str, options: &Options) -> Ingested {
    // The cap flag is deliberately dropped here, unlike in `web`: a watch page cut short
    // loses the caption track, and *that* is already reported as the partial outcome below.
    // Reporting the byte cut as well would name the mechanism instead of the consequence.
    let (page, _cut) = match get_capped(source, options, YOUTUBE_MAX_BYTES) {
        Ok(page) => page,
        Err(message) => return Ingested::failed(source, Kind::YouTube, "YouTube", message),
    };

    let title = html_title(&page)
        .map(|title| strip_youtube_suffix(&title))
        .unwrap_or_else(|| "YouTube video".to_owned());

    let captions = caption_track_url(&page)
        .and_then(|url| get_capped(&url, options, WEB_MAX_BYTES).ok())
        .map(|(xml, _)| timedtext_to_text(&xml))
        .filter(|text| !text.trim().is_empty());

    let text = match &captions {
        Some(captions) => format!("{title}\n{source}\n\n{captions}"),
        None => format!("{title}\n{source}"),
    };
    let chars = text.chars().count();
    let outcome = match captions {
        Some(_) => Outcome::Extracted { chars },
        // Not a failure — the title *is* context, and the link is attached either way. But
        // it is reported as partial, because "attached" on its own would let a user believe
        // an agent had been given what was said in the video.
        None => Outcome::Partial {
            chars,
            message: "YouTube would not serve this video's caption track, so only its title \
                      was attached."
                .to_owned(),
        },
    };
    Ingested::new(source, Kind::YouTube, title, text, outcome)
}

fn is_youtube(lowercase_url: &str) -> bool {
    host_of(lowercase_url).is_some_and(|host| {
        let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
        host == "youtube.com" || host == "youtu.be" || host == "m.youtube.com"
    })
}

/// The host of an http(s) URL, lowercased, without credentials or a port.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = authority.split(':').next()?;
    if host.is_empty() { None } else { Some(host.to_lowercase()) }
}

/// The `v=` id of a watch URL, or the path of a `youtu.be` one.
///
/// Not used by the ingestion itself — the whole page is fetched by URL — but it is what a
/// caller needs to draw a poster frame or to tell two attachments of the same video apart.
pub fn youtube_id(url: &str) -> Option<String> {
    let host = host_of(url)?;
    let host = host.strip_prefix("www.").unwrap_or(&host);
    // Gated on the host, not merely on the shape of the query. Every second site on the web
    // has a `?v=` parameter, and answering with one of those would attach the wrong video.
    match host {
        "youtu.be" => {
            let path = url.split_once("://")?.1.split_once('/')?.1;
            let id = path.split(['?', '#', '/']).next()?;
            (!id.is_empty()).then(|| id.to_owned())
        }
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => url
            .split_once('?')?
            .1
            .split('&')
            .find_map(|pair| pair.strip_prefix("v="))
            .map(|id| id.split('#').next().unwrap_or(id).to_owned())
            .filter(|id| !id.is_empty()),
        _ => None,
    }
}

fn strip_youtube_suffix(title: &str) -> String {
    title.trim().strip_suffix(" - YouTube").unwrap_or(title.trim()).to_owned()
}

/// The first caption track's URL, out of the player payload embedded in a watch page.
///
/// The `baseUrl` arrives JSON-escaped — **`&` for every `&`**, measured in the real
/// page — so fetching it verbatim requests a URL with no query parameters at all and gets
/// nothing back. [`unescape_json`] is not optional here.
pub fn caption_track_url(page: &str) -> Option<String> {
    let at = page.find("\"captionTracks\"")?;
    let rest = page.get(at..)?;
    let key = rest.find("\"baseUrl\":\"")?;
    let value = rest.get(key + "\"baseUrl\":\"".len()..)?;
    let end = value.find('"')?;
    let raw = value.get(..end)?;
    Some(unescape_json(raw))
}

/// YouTube's default timedtext format: `<text start=… dur=…>one line</text>` repeated.
///
/// **Not verified against a live response**, because the endpoint returns nothing to a plain
/// client (see [`youtube`]). It is written against the documented shape and tested on a
/// literal. The `srv3` variant, which uses `<p>` elements, is not handled.
pub fn timedtext_to_text(xml: &str) -> String {
    let mut out = String::new();
    for chunk in xml.split("</text>") {
        let Some(at) = chunk.rfind('>') else { continue };
        let Some(content) = chunk.get(at + 1..) else { continue };
        let line = decode_entities(content).trim().to_owned();
        if !line.is_empty() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    normalise_text(&out)
}

// ---------------------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------------------

/// Tags whose *content* is not text a reader sees.
const SKIPPED: [&str; 7] = ["script", "style", "noscript", "svg", "template", "head", "iframe"];

/// Tags that end a line.
const BREAKING: [&str; 24] = [
    "p", "br", "div", "li", "ul", "ol", "tr", "td", "th", "h1", "h2", "h3", "h4", "h5", "h6",
    "section", "article", "header", "footer", "nav", "blockquote", "figure", "hr", "table",
];

/// A page's `<title>`, decoded and trimmed.
pub fn html_title(html: &str) -> Option<String> {
    let chars: Vec<char> = html.chars().collect();
    let open = find_ci(&chars, "<title", 0)?;
    let close = chars.get(open..)?.iter().position(|ch| *ch == '>')? + open + 1;
    let end = find_ci(&chars, "</title", close)?;
    let raw: String = chars.get(close..end)?.iter().collect();
    let title = decode_entities(&raw).trim().to_owned();
    (!title.is_empty()).then_some(title)
}

/// A small, tolerant HTML-to-text pass.
///
/// **What it does**: drops the content of `<script>`, `<style>`, `<noscript>`, `<svg>`,
/// `<template>`, `<head>` and `<iframe>`; drops comments and doctypes; turns block-level tags
/// into line breaks; removes every other tag; decodes entities; and collapses runs of
/// whitespace and blank lines.
///
/// **What it does not do**: run JavaScript, so a page drawn client-side yields nothing and
/// says so; apply CSS, so a `display:none` block is read as though it were visible; preserve
/// `<pre>` whitespace; lay out a table as columns; understand `<base href>`; or resolve
/// character references that are not in [`decode_entities`]' table. It also does not care
/// about mis-nested tags, because it never builds a tree — which is exactly why a real page
/// does not break it.
pub fn html_to_text(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let mut out = String::with_capacity(html.len() / 2);
    let mut index = 0usize;

    while index < chars.len() {
        if chars[index] != '<' {
            out.push(chars[index]);
            index += 1;
            continue;
        }

        // A comment or a doctype. `<!--` runs to `-->`; anything else `<!` runs to `>`.
        if chars.get(index + 1) == Some(&'!') {
            index = if chars.get(index + 2..index + 4) == Some(&['-', '-'][..]) {
                find_ci(&chars, "-->", index).map_or(chars.len(), |at| at + 3)
            } else {
                skip_tag(&chars, index)
            };
            continue;
        }

        let name = tag_name(&chars, index);
        let after = skip_tag(&chars, index);
        let closing = chars.get(index + 1) == Some(&'/');

        if !closing && SKIPPED.contains(&name.as_str()) {
            // Everything up to the matching close. A missing close eats the rest, which is
            // the right answer for an unterminated `<script>` and the wrong one for nothing.
            index = find_ci(&chars, &format!("</{name}"), after)
                .map_or(chars.len(), |at| skip_tag(&chars, at));
            continue;
        }
        if BREAKING.contains(&name.as_str()) {
            out.push('\n');
        }
        index = after;
    }

    normalise_text(&decode_entities(&out))
}

/// The lowercase tag name at `<`, without its slash.
fn tag_name(chars: &[char], at: usize) -> String {
    let mut index = at + 1;
    if chars.get(index) == Some(&'/') {
        index += 1;
    }
    let mut name = String::new();
    while let Some(ch) = chars.get(index) {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_lowercase());
            index += 1;
        } else {
            break;
        }
    }
    name
}

/// The index just past this tag's `>`, honouring quoted attribute values so that a `>`
/// inside one — `alt="a > b"` — does not end the tag early.
fn skip_tag(chars: &[char], at: usize) -> usize {
    let mut index = at + 1;
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.get(index) {
        match quote {
            Some(open) if *ch == open => quote = None,
            Some(_) => {}
            None if *ch == '"' || *ch == '\'' => quote = Some(*ch),
            None if *ch == '>' => return index + 1,
            None => {}
        }
        index += 1;
    }
    chars.len()
}

/// Decodes HTML character references in **one left-to-right pass**.
///
/// The output is appended to and never re-examined, so a `&` this function *produces* is not
/// a candidate for the next match. That is the shape rather than the ordering, and it is
/// deliberate: `vellum-link` shipped a table applied as a sequence of `str::replace` with a
/// comment saying `&amp;` must come last — and `&#38;` sat after it, so `&amp;#38;` decoded
/// twice (feedback 30). A pass that cannot re-read its own output cannot have that bug.
///
/// The table is the references that actually occur in page titles and prose, plus numeric
/// references in both decimal and hex. An unknown reference is left exactly as written,
/// which is better than dropping it: `&foo;` in the text is legible, and nothing is lost.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;

    while index < chars.len() {
        if chars[index] == '&' {
            // A reference is short. Bounding the search stops a bare `&` in prose from
            // scanning to the end of the document for every occurrence.
            let limit = (index + 12).min(chars.len());
            if let Some(offset) = chars.get(index + 1..limit).and_then(|window| {
                window.iter().position(|ch| *ch == ';')
            }) {
                let end = index + 1 + offset;
                let name: String = chars.get(index + 1..end).map(|s| s.iter().collect()).unwrap_or_default();
                if let Some(decoded) = entity(&name) {
                    out.push(decoded);
                    index = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(digits) = name.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        // A non-breaking space becomes an ordinary one on purpose: this text is going into a
        // prompt, where U+00A0 is a character an agent has to spend attention on and a space
        // is what the page meant.
        "nbsp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "times" => '×',
        "laquo" => '«',
        "raquo" => '»',
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

/// Collapses whitespace the way a reader would: no trailing spaces, no runs of blank lines,
/// no leading or trailing emptiness. Runs of spaces and tabs inside a line become one space.
fn normalise_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let mut collapsed = String::with_capacity(line.len());
        let mut space = false;
        for ch in line.chars() {
            if ch.is_whitespace() {
                space = true;
            } else {
                if space && !collapsed.is_empty() {
                    collapsed.push(' ');
                }
                space = false;
                collapsed.push(ch);
            }
        }
        if collapsed.is_empty() {
            blank_run += 1;
            // One blank line survives as a paragraph break; the rest are the page's layout
            // leaking into the prompt.
            if blank_run == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        out.push_str(&collapsed);
        out.push('\n');
    }
    out.trim().to_owned()
}

/// JSON string escapes, over characters. Handles `\uXXXX`, which is the one that matters
/// here — YouTube writes every `&` in a caption URL as `&`.
fn unescape_json(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '\\' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        match chars.get(index + 1).copied() {
            Some('u') => {
                let hex: String =
                    chars.get(index + 2..index + 6).map(|s| s.iter().collect()).unwrap_or_default();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => {
                        out.push(ch);
                        index += 6;
                    }
                    None => {
                        out.push('\\');
                        index += 1;
                    }
                }
            }
            Some('n') => {
                out.push('\n');
                index += 2;
            }
            Some('t') => {
                out.push('\t');
                index += 2;
            }
            Some(other) => {
                out.push(other);
                index += 2;
            }
            None => {
                out.push('\\');
                index += 1;
            }
        }
    }
    out
}

/// Whether a command exists on `PATH`, without running it.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
    })
}

/// Runs a converter over a file and returns its stdout.
///
/// `stdout_dash` is for `pdftotext`, whose output file argument must be `-` to write to
/// stdout — `textutil` takes `-stdout` as a flag instead. One boolean rather than two
/// functions, because everything else about the two calls is identical.
fn run_tool(
    tool: &'static str,
    args: &[&str],
    path: &Path,
    stdout_dash: bool,
) -> std::result::Result<String, String> {
    let mut command = Command::new(tool);
    command.args(args).arg(path);
    if stdout_dash {
        command.arg("-");
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() { format!("`{tool}` failed") } else { stderr });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A capped, timed-out GET. The call shape is `vellum-link`'s, deliberately unchanged.
///
/// Returns the body **and whether the cap cut it**, read one byte past the cap so that "there
/// was more" and "there was exactly this much" are distinguishable — the same arrangement
/// [`text_file`] uses, and for the same reason: a page reported as read whole when a third of
/// it was dropped is a claim the node has no business making.
fn get_capped(
    url: &str,
    options: &Options,
    cap: usize,
) -> std::result::Result<(String, bool), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("`{url}` is not an http(s) address."));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(options.timeout))
        .user_agent(USER_AGENT)
        .build()
        .into();

    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("{url} could not be reached: {error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url} answered {status}."));
    }

    // `take` rather than trusting `Content-Length`: it is a claim, and a chunked response
    // does not make one at all.
    let mut body = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(cap.saturating_add(1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("reading {url} failed: {error}"))?;
    let truncated = body.len() > cap;
    if truncated {
        body.truncate(cap);
    }
    Ok((String::from_utf8_lossy(&body).into_owned(), truncated))
}

/// Case-insensitive search over a character slice.
///
/// Over `&[char]` rather than over a lowercased `String`, because `str::to_lowercase` can
/// change a string's *length* — `İ` becomes two characters — so an index found in the
/// lowercased copy does not address the same place in the original. That is a silent
/// off-by-a-few on any page containing one Turkish capital.
fn find_ci(haystack: &[char], needle: &str, from: usize) -> Option<usize> {
    let needle: Vec<char> = needle.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| {
            window.iter().zip(needle.iter()).all(|(a, b)| a.to_ascii_lowercase() == *b)
        })
        .map(|at| at + from)
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack.get(from..)?.windows(needle.len()).position(|window| window == needle).map(|at| at + from)
}

fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).rposition(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn offline() -> Options {
        // `Tools::none` is what makes the degraded messages testable: with a real probe these
        // assertions would pass or fail depending on whether the machine has poppler.
        Options { tools: Tools::none(), ..Options::default() }
    }

    #[test]
    fn a_source_is_classified_by_its_spelling_alone() {
        assert_eq!(classify("notes.md"), Kind::Text);
        assert_eq!(classify("/a/b/SPEC.PDF"), Kind::Pdf, "the extension match must fold case");
        assert_eq!(classify("report.docx"), Kind::Docx);
        assert_eq!(classify("memo.m4a"), Kind::Audio);
        assert_eq!(classify("clip.mov"), Kind::Video);
        assert_eq!(classify("photo.heic"), Kind::Image);
        assert_eq!(classify("Makefile"), Kind::Unknown, "a file with no extension is sniffed");
        assert_eq!(classify("https://example.com/a"), Kind::Web);
        assert_eq!(classify("https://www.youtube.com/watch?v=abc"), Kind::YouTube);
        assert_eq!(classify("https://youtu.be/abc"), Kind::YouTube);
        assert_eq!(classify("https://m.youtube.com/watch?v=abc"), Kind::YouTube);
        // A host that merely *contains* the word is not YouTube — this is the check a
        // `contains("youtube.com")` implementation fails.
        assert_eq!(classify("https://notyoutube.com.evil.example/a"), Kind::Web);

        // The tags are written into board files, so they are pinned.
        assert_eq!(Kind::Unknown.tag(), "text");
        assert_eq!(Kind::YouTube.tag(), "youtube");
    }

    #[test]
    fn a_text_file_is_read_and_a_binary_one_is_named_rather_than_mangled() {
        let temp = tempfile::tempdir().unwrap();
        let notes = temp.path().join("notes.md");
        std::fs::write(&notes, "# Plan\n\nDo the thing.").unwrap();

        let read = ingest_with(notes.to_str().unwrap(), &offline());
        assert!(read.outcome.is_text(), "{:?}", read.outcome);
        assert_eq!(read.text, "# Plan\n\nDo the thing.");
        assert_eq!(read.source.kind, "text");
        assert_eq!(read.source.label, "notes.md");
        assert_eq!(read.source.extract, None, "this crate cannot hash; the caller fills that in");

        let binary = temp.path().join("mystery");
        std::fs::write(&binary, [0x00, 0x01, 0x02, 0xFF]).unwrap();
        let refused = ingest_with(binary.to_str().unwrap(), &offline());
        assert!(!refused.outcome.is_text());
        assert!(refused.outcome.message().contains("does not look like text"), "{:?}", refused.outcome);
    }

    /// Invalid UTF-8 must be lossily converted, never a panic — the release profile is
    /// `panic = "abort"`, so a panic here takes the whole application with it, and an
    /// attached file is arbitrary bytes by definition.
    #[test]
    fn invalid_utf8_is_replaced_rather_than_fatal() {
        let temp = tempfile::tempdir().unwrap();
        let latin = temp.path().join("latin.txt");
        // `café` in Latin-1: the 0xE9 is not valid UTF-8 on its own.
        std::fs::write(&latin, [b'c', b'a', b'f', 0xE9, b'\n']).unwrap();
        let read = ingest_with(latin.to_str().unwrap(), &offline());
        assert!(read.outcome.is_text());
        assert!(read.text.starts_with("caf"), "{:?}", read.text);
        assert!(read.text.contains('\u{FFFD}'), "the bad byte was not replaced: {:?}", read.text);
    }

    /// The cap has to cut on a byte boundary, which can be the middle of a character. The
    /// lossy conversion is what makes that safe, and this is the test that says so.
    #[test]
    fn a_file_larger_than_the_cap_is_truncated_and_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let big = temp.path().join("big.txt");
        let mut file = std::fs::File::create(&big).unwrap();
        // Three-byte characters, so the cap of 100 lands inside one.
        for _ in 0..200 {
            file.write_all("夕".as_bytes()).unwrap();
        }
        drop(file);

        let options = Options { max_file_bytes: 100, ..offline() };
        let read = ingest_with(big.to_str().unwrap(), &options);
        assert!(matches!(read.outcome, Outcome::Partial { .. }), "{:?}", read.outcome);
        assert!(read.outcome.is_text(), "a truncated file still has text in it");
        assert!(read.outcome.message().contains("longer"), "{}", read.outcome.message());
        assert!(read.text.starts_with('夕'));
        // 100 bytes is 33 whole characters and one third of a fourth. The lossy conversion
        // is what turns that last third into one replacement character instead of a panic.
        assert_eq!(read.text.chars().count(), 34, "{:?}", read.text);
        assert!(read.text.ends_with('\u{FFFD}'), "the split character was not replaced");
    }

    /// Audio is not decoded here at all. The hand-off is the contract with the transcription
    /// path, and a test that only checked "no text came out" would pass on a build that
    /// dropped the file on the floor.
    #[test]
    fn audio_is_handed_to_the_transcription_path_rather_than_read() {
        let temp = tempfile::tempdir().unwrap();
        let memo = temp.path().join("memo.m4a");
        std::fs::write(&memo, [0u8; 32]).unwrap();

        let handed = ingest_with(memo.to_str().unwrap(), &offline());
        assert_eq!(handed.source.kind, "audio");
        match &handed.outcome {
            Outcome::Transcribe(handoff) => {
                assert_eq!(handoff.kind, Kind::Audio);
                assert_eq!(handoff.path, memo, "the hand-off must carry the file to transcribe");
            }
            other => panic!("audio was not handed off: {other:?}"),
        }
        assert!(handed.outcome.message().contains("transcription"));
        assert!(handed.text.is_empty());
    }

    /// A folder is not a failure and not a silent nothing: it is the thing a file-tree node
    /// exists for, and the message says so.
    #[test]
    fn a_folder_is_pointed_at_the_node_kind_built_for_it() {
        let temp = tempfile::tempdir().unwrap();
        let outcome = ingest_with(temp.path().to_str().unwrap(), &offline()).outcome;
        assert!(outcome.message().contains("file-tree"), "{outcome:?}");
    }

    /// Every outcome has something to say. This is the rule that nothing is inert, asserted
    /// over the whole enum rather than one variant at a time — a new variant with an empty
    /// message fails here.
    #[test]
    fn no_outcome_is_ever_silent() {
        let outcomes = [
            Outcome::Extracted { chars: 10 },
            Outcome::Partial { chars: 10, message: "half of it".into() },
            Outcome::NeedsTool { tool: "pdftotext", message: "install poppler".into() },
            Outcome::Transcribe(MediaHandoff { path: "/a.m4a".into(), kind: Kind::Audio }),
            Outcome::Transcribe(MediaHandoff { path: "/a.mp4".into(), kind: Kind::Video }),
            Outcome::Unsupported { message: "no reader".into() },
            Outcome::Failed { message: "gone".into() },
        ];
        for outcome in outcomes {
            assert!(!outcome.message().trim().is_empty(), "{outcome:?} said nothing");
        }
    }

    #[test]
    fn a_missing_file_names_itself() {
        let outcome = ingest_with("/no/such/file.txt", &offline()).outcome;
        assert!(matches!(outcome, Outcome::Failed { .. }));
        assert!(outcome.message().contains("/no/such/file.txt"), "{outcome:?}");
        assert!(!ingest_with("   ", &offline()).outcome.message().is_empty());
    }

    /// A hand-built PDF with an uncompressed content stream — the case the built-in
    /// extractor exists for. The escapes and the two `Td`s are in there because they are
    /// what separates a real extractor from one that greps for brackets.
    #[test]
    fn an_uncompressed_pdf_gives_up_its_text() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Length 90 >>\nstream\nBT /F1 12 Tf 72 720 Td \
                    (Hello, agent.) Tj 0 -14 Td (Line \\(two\\)) Tj ET\nendstream\nendobj\n\
                    trailer\n%%EOF\n";
        match pdf_text(pdf) {
            PdfText::Text(text) => {
                assert_eq!(text, "Hello, agent.\nLine (two)", "{text:?}");
            }
            other => panic!("the uncompressed stream was not read: {other:?}"),
        }
    }

    /// Hex strings, the UTF-16BE byte-order mark and the WinAnsi high block — the three
    /// encodings a real document actually uses. A `char::from(byte)` implementation gets the
    /// last one wrong and turns every curly apostrophe into a control character.
    #[test]
    fn pdf_strings_decode_hex_utf16_and_the_windows_high_block() {
        let hex = b"%PDF-1.4\n<< >>\nstream\nBT <48656C6C6F> Tj ET\nendstream\n";
        assert_eq!(pdf_text(hex), PdfText::Text("Hello".into()));

        let utf16 = b"%PDF-1.4\n<< >>\nstream\nBT <FEFF00480069> Tj ET\nendstream\n";
        assert_eq!(pdf_text(utf16), PdfText::Text("Hi".into()));

        // 0x92 is a right single quote in WinAnsi and a control character in Latin-1.
        assert_eq!(pdf_string_to_text(&[b'i', b't', 0x92, b's']), "it\u{2019}s");
        assert_eq!(pdf_string_to_text(&[0x85]), "\u{2026}", "the ellipsis byte");
        assert_eq!(pdf_string_to_text(&[0xE9]), "é", "Latin-1 must survive outside the high block");
    }

    /// The common case, and the one this module has to be honest about: a compressed PDF
    /// cannot be read here, and the answer names the tool that would.
    #[test]
    fn a_compressed_pdf_names_the_tool_it_needs_instead_of_returning_nothing() {
        let pdf = b"%PDF-1.5\n1 0 obj\n<< /Length 20 /Filter /FlateDecode >>\nstream\n\
                    \x78\x9c\x01\x02\x03\x04\nendstream\nendobj\n";
        assert_eq!(pdf_text(pdf), PdfText::Compressed { streams: 1 });

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("spec.pdf");
        std::fs::write(&path, pdf).unwrap();

        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        assert!(!ingested.outcome.is_text());
        assert!(ingested.text.is_empty(), "a refusal must not pretend to have text");
        match &ingested.outcome {
            Outcome::NeedsTool { tool, message } => {
                assert_eq!(*tool, "pdftotext");
                assert!(message.contains("pdftotext"), "{message}");
                assert!(message.contains("poppler"), "the message must say how to get it: {message}");
            }
            other => panic!("a compressed PDF did not name the tool: {other:?}"),
        }
    }

    #[test]
    fn a_pdf_with_no_text_and_a_file_that_is_not_one_are_told_apart() {
        assert_eq!(pdf_text(b"not a pdf at all"), PdfText::NotPdf);
        assert_eq!(pdf_text(b"%PDF-1.4\n<< >>\nstream\n0 0 m 10 10 l S\nendstream\n"), PdfText::NoText);
    }

    /// Without `textutil` this is a refusal, and the refusal has to be actionable. Driven
    /// through `Tools::none` rather than through whatever the test machine happens to have.
    #[test]
    fn a_word_document_without_a_converter_says_what_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.docx");
        std::fs::write(&path, b"PK\x03\x04 not really").unwrap();

        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        match &ingested.outcome {
            Outcome::NeedsTool { tool, message } => {
                assert_eq!(*tool, "textutil");
                assert!(message.contains("textutil"), "{message}");
                assert!(message.contains(".txt"), "the message must offer a way out: {message}");
            }
            other => panic!("a docx with no converter did not name one: {other:?}"),
        }
    }

    /// The shell-out, end to end, on a machine that has the converter — skipped elsewhere,
    /// exactly as the git tests in [`crate::worktree`] are. Without it `run_tool` is the one
    /// function in this module with no coverage at all, and it *is* the whole implementation
    /// of `.docx` on the platform Velm ships on first.
    ///
    /// The fixture is built with the same tool that reads it, which is the only way to get a
    /// genuine `.docx` into a test without committing a binary blob to the repository.
    #[test]
    fn a_word_document_is_read_when_the_converter_is_installed() {
        if !Tools::probe().textutil {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src.txt");
        let document = temp.path().join("report.docx");
        std::fs::write(&source, "Torque figures for the swap.\nSecond line.\n").unwrap();
        let made = Command::new("textutil")
            .args(["-convert", "docx", "-output"])
            .arg(&document)
            .arg(&source)
            .status()
            .expect("textutil should run");
        assert!(made.success(), "the fixture could not be built");

        let ingested = ingest_with(document.to_str().unwrap(), &Options::default());
        assert!(ingested.outcome.is_text(), "{:?}", ingested.outcome);
        assert_eq!(ingested.source.kind, "docx");
        assert!(
            ingested.text.contains("Torque figures for the swap."),
            "the converter's output did not reach the agent: {:?}",
            ingested.text
        );
    }

    #[test]
    fn html_becomes_readable_text_and_loses_its_scripts() {
        let html = "<!doctype html><html><head><title>Torque &amp; Swaps</title>\
                    <style>body{color:red}</style></head><body>\
                    <script>var a = 1 < 2;</script>\
                    <h1>Heading</h1><p>First   paragraph.</p><p>Second &mdash; with a dash.</p>\
                    <div><img alt=\"a > b\">Trailing</div></body></html>";

        assert_eq!(html_title(html).as_deref(), Some("Torque & Swaps"));

        let text = html_to_text(html);
        assert!(!text.contains("color:red"), "the stylesheet leaked: {text}");
        assert!(!text.contains("var a"), "the script leaked: {text}");
        assert!(!text.contains('<'), "a tag survived: {text}");
        assert!(text.contains("Heading"));
        assert!(text.contains("First paragraph."), "runs of spaces were not collapsed: {text}");
        assert!(text.contains("Second — with a dash."));
        // The `>` inside a quoted attribute must not have ended the tag early.
        assert!(text.contains("Trailing"), "{text}");
        assert!(!text.contains("a > b"), "an attribute value leaked into the text: {text}");
    }

    /// The exact bug `vellum-link` shipped: a table applied as a sequence of replacements
    /// decodes `&amp;#38;` twice. One left-to-right pass cannot, because the `&` it writes is
    /// never looked at again — this assertion fails on the sequential implementation and
    /// passes on this one.
    #[test]
    fn an_entity_the_decoder_produces_is_never_decoded_again() {
        assert_eq!(decode_entities("&amp;#38;"), "&#38;");
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("wouldn&#39;t"), "wouldn't");
        assert_eq!(decode_entities("&#x2014;"), "—");
        assert_eq!(decode_entities("caf&eacute;"), "caf&eacute;", "an unknown reference is kept");
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry", "a bare ampersand is left alone");
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
        assert_eq!(decode_entities("no entities here"), "no entities here");
        // Multi-byte text around a reference — the shape that aborted this codebase twice.
        assert_eq!(decode_entities("夕&amp;焼け"), "夕&焼け");
        assert_eq!(decode_entities("&"), "&");
        assert_eq!(decode_entities("&;"), "&;");
    }

    /// The caption URL arrives with `&` for every `&` — measured in a real watch page.
    /// Fetching it without this step requests a URL with no query parameters at all.
    #[test]
    fn a_caption_track_url_is_unescaped_before_it_is_fetched() {
        // The escapes are written as YouTube writes them — `&` for every `&`, copied
        // from the real page. A fixture with plain ampersands would pass against an
        // implementation that skipped the unescaping entirely, which is the whole bug.
        let page = r#"{"playerCaptionsTracklistRenderer":{"captionTracks":[{"baseUrl":"https://www.youtube.com/api/timedtext?v=x\u0026lang=en\u0026caps=asr","name":{"simpleText":"English"}}]}}"#;
        assert_eq!(
            caption_track_url(page).as_deref(),
            Some("https://www.youtube.com/api/timedtext?v=x&lang=en&caps=asr")
        );
        assert_eq!(caption_track_url("{}"), None, "a page with no tracks must not invent one");
        assert_eq!(unescape_json(r"a\/b&c"), "a/b&c");
    }

    #[test]
    fn a_youtube_title_loses_the_suffix_the_page_adds() {
        let page = "<html><head><title>Rick Astley - Never Gonna Give You Up - YouTube</title>\
                    </head><body></body></html>";
        let title = html_title(page).map(|title| strip_youtube_suffix(&title));
        assert_eq!(title.as_deref(), Some("Rick Astley - Never Gonna Give You Up"));
        // A title that merely contains the word keeps every character of it.
        assert_eq!(strip_youtube_suffix("How YouTube works"), "How YouTube works");
    }

    #[test]
    fn a_video_id_comes_out_of_both_url_shapes() {
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(youtube_id("https://youtu.be/dQw4w9WgXcQ?t=30").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(youtube_id("https://www.youtube.com/watch?list=x&v=abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://example.com/watch?v=abc"), None);
        assert_eq!(host_of("https://user:pw@Example.COM:8443/a?b").as_deref(), Some("example.com"));
    }

    #[test]
    fn a_caption_document_becomes_lines() {
        let xml = "<?xml version=\"1.0\"?><transcript>\
                   <text start=\"0\" dur=\"2\">Never gonna give you up</text>\
                   <text start=\"2\" dur=\"2\">Never gonna let you &amp;down</text>\
                   <text start=\"4\" dur=\"1\">   </text></transcript>";
        assert_eq!(
            timedtext_to_text(xml),
            "Never gonna give you up\nNever gonna let you &down",
            "an empty cue must not become a blank line"
        );
    }

    #[test]
    fn whitespace_is_collapsed_the_way_a_reader_would() {
        assert_eq!(normalise_text("  a  \n\n\n\n  b  \n"), "a\n\nb");
        assert_eq!(normalise_text("\n\n\n"), "");
        assert_eq!(normalise_text("one\ttwo   three"), "one two three");
    }
}
