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
//! # What is read here, and what still needs a tool
//!
//! `flate2` and `zip` are dependencies, so the two formats that matter are read in-process
//! and on every platform:
//!
//! - **PDF** — `FlateDecode`d content streams are inflated and tokenised, which is nearly
//!   every real document. `pdftotext` is no longer the answer for an ordinary PDF; it stays
//!   as the *better* reader where it is installed, because it resolves font encodings and
//!   `/ToUnicode` maps and this extractor does not.
//! - **`.docx`, `.pptx`, `.xlsx`** — a ZIP of XML, read directly. **These do not need
//!   `textutil`**, which is the whole point: `textutil` is macOS-only, so relying on it meant
//!   Word documents working on the primary target and nowhere else.
//!
//! `textutil` is kept for the formats that are *not* ZIP-of-XML — `.doc`, `.rtf`, `.odt`,
//! `.pages` — where it is the only reader available, and as a fallback for an Office file
//! this crate cannot open. Those degrade to a named message off macOS.
//!
//! **The tool probe is injectable** ([`Tools::none`]) so a degraded message is an ordinary
//! unit test rather than something only reproducible on a machine that lacks the tool.
//!
//! # Compressed input is bounded
//!
//! Everything decompressed here comes from a file the user was given, so every inflate is
//! capped. A zip bomb is a hundred kilobytes on disk and a terabyte in memory, and this crate
//! runs inside the application's own process.
//!
//! ⚠ **Two caps, per item and in aggregate, and this paragraph used to name only the first
//! pair.** [`MAX_INFLATED_BYTES`] bounds one PDF stream and [`MAX_PART_BYTES`] bounds one ZIP
//! entry — and neither bounds a *file*, because a PDF has as many streams as it likes and an
//! Office document is *made* of parts, all of which were accumulated with nothing counting the
//! total. Measured against the code as it stood: ~128 PDF objects × 64MB ≈ 7.9GB, and 100
//! spreadsheet sheets × 32MB ≈ 3.2GB, both from an input of a few hundred kilobytes.
//! [`MAX_PDF_TEXT_BYTES`] and [`MAX_PARTS_BYTES`] are the totals, and they are what makes the
//! sentence above true.
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

/// The most any single PDF stream may inflate to.
///
/// A bound on *someone else's* file, not a tuning knob. A page of text is a few kilobytes
/// inflated; 64MB is four orders of magnitude of headroom and still refuses the classic
/// deflate bomb, which is a few hundred kilobytes on disk and unbounded in memory.
pub const MAX_INFLATED_BYTES: usize = 64 * 1024 * 1024;

/// The most any single part of an Office file may be read as.
///
/// `word/document.xml` for a long report is a few megabytes; a `.xlsx` sheet with a hundred
/// thousand rows is larger, which is why this is not smaller.
///
/// ⚠ **On its own this is not a bound on the file** — see [`MAX_PARTS_BYTES`].
pub const MAX_PART_BYTES: usize = 32 * 1024 * 1024;

/// The most **every** part of one Office file may come to, added together.
///
/// ⚠ A per-part cap is not a cap on a document, and a `.pptx` or a `.xlsx` is *made* of parts:
/// [`office_parts_within`] collects every matching entry into one `Vec` before a caller sees
/// any of them, so a workbook of a hundred sheets was a hundred × [`MAX_PART_BYTES`] — 3.2GB —
/// held at once, from an archive that is a few hundred kilobytes on disk. That is the classic
/// ZIP bomb with the per-entry check passing on every entry.
///
/// A whole Office document worth reading is a few megabytes of XML; 64MB is a document nobody
/// is going to read to the end of. A part that does not fit in what is left is **dropped whole
/// rather than truncated**, because a half-read XML part parses as a shorter document rather
/// than as a broken one — and the parts *behind* it are still read, which is not a detail:
/// Excel writes its string table last, and dropping the rest of the archive with the first
/// oversized sheet took every word in the workbook with it. Either way the caller is told, and
/// [`office`] turns that into [`Outcome::Partial`] rather than a complete read.
pub const MAX_PARTS_BYTES: usize = 64 * 1024 * 1024;

/// The most text one PDF will yield, across **every** content stream in it.
///
/// ⚠ The same shape as [`MAX_PARTS_BYTES`], and the same reason. [`MAX_INFLATED_BYTES`] bounds
/// one stream; a PDF has as many streams as it likes, and `pdf_text` concatenated all of them
/// into one `String` with nothing counting the total. A document with 128 objects — an
/// unremarkable number — was 128 × 64MB, near 8GB, in a process that also holds a GPU surface
/// on an 8GB machine.
///
/// 8MB of extracted text is roughly four thousand pages. A cap on someone else's file, not a
/// tuning knob.
pub const MAX_PDF_TEXT_BYTES: usize = 8 * 1024 * 1024;

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
    /// A Word document. Read here, on every platform.
    Docx,
    /// A PowerPoint deck. Read here: the same ZIP of XML, one part per slide.
    Pptx,
    /// An Excel workbook. Read here, as tab-separated rows.
    Xlsx,
    /// A word processor document that is **not** ZIP-of-XML — `.doc`, `.rtf`, `.odt`,
    /// `.pages`. Its own kind because it is the only family left that needs an external
    /// converter, and lumping it in with `.docx` would have made the message for both of
    /// them wrong: one of them now always works.
    LegacyDoc,
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
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
            Self::LegacyDoc => "document",
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
            // ⚠ **This used to say "queued for transcription", and nothing queued it.**
            // `Outcome::Transcribe` is produced here and consumed nowhere: `voice`'s
            // transcribers take an [`crate::voice::Utterance`] — captured samples — and a
            // media file is a container that has to be decoded first, which nothing in this
            // workspace does. So the honest sentence names the file, says what the agent gets,
            // and says what it does not. The house rule is that a known gap is fine and a
            // false claim is not; *"queued"* was the second, and it was the kind a user only
            // discovers by waiting for a transcript that was never coming.
            // ⚠ **This sentence is what is left when transcription did *not* happen**, and it
            // used to say Velm cannot transcribe media at all. It can now, with a transcriber
            // installed on this machine — `voice::transcribe_media`, called by the app, turns
            // this outcome into `Extracted` and this message is never reached. So the honest
            // wording is conditional on the machine rather than on the product.
            Self::Transcribe(handoff) => format!(
                "Attached as {}. Nothing on this machine could turn it into words, so the \
                 agent is given the file's path rather than what was said.",
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
        "docx" => Kind::Docx,
        "pptx" => Kind::Pptx,
        "xlsx" => Kind::Xlsx,
        // `.docm`/`.pptm`/`.xlsm` are the same ZIP with macros in it, and the text parts are
        // identical — so they are read by the same path rather than refused for carrying a
        // macro this crate never executes.
        "docm" => Kind::Docx,
        "pptm" => Kind::Pptx,
        "xlsm" => Kind::Xlsx,
        "doc" | "rtf" | "odt" | "pages" => Kind::LegacyDoc,
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
        Kind::Docx | Kind::Pptx | Kind::Xlsx => office(source, &label, path, kind, options),
        Kind::LegacyDoc => legacy_document(source, &label, path, options),
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
        PdfText::Unreadable { streams } => Ingested::new(
            source,
            Kind::Pdf,
            label,
            String::new(),
            Outcome::NeedsTool {
                tool: "pdftotext",
                message: format!(
                    "{label} keeps its text in {streams} stream(s) that Velm could not read — \
                     an older compression, or a damaged file. `pdftotext` reads more than \
                     this does (poppler: `brew install poppler`); with it installed, attach \
                     the file again."
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
    /// Content streams that could not be read. **No longer the ordinary case**: `FlateDecode`
    /// is inflated in-process, so this is now only LZW compression, a `/Predictor` this
    /// extractor will not guess at, or a stream too damaged to inflate. The count is reported
    /// so the message can be specific about how much was lost.
    Unreadable { streams: usize },
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
/// **`FlateDecode` is inflated** ([`inflate`]), which is the difference between reading the
/// uncompressed minority of PDFs and reading nearly all of them — producers have emitted
/// compressed content streams by default for twenty years.
///
/// **What it still does not do.** It does not resolve font encodings or `/ToUnicode` maps, so
/// a document set in a subset-encoded font comes out as the wrong letters — and there is no
/// way to notice that from in here, which is the most important limitation on this list
/// because it fails *silently*. It does not un-apply a `/Predictor`, does not do LZW, does
/// not decrypt, does not follow object streams to find content, and makes no attempt at
/// reading order across columns or at telling a header from a paragraph.
///
/// So `pdftotext` remains the better reader where it exists, and [`pdf`] prefers it. This is
/// the honest in-process answer, not a PDF library.
pub fn pdf_text(bytes: &[u8]) -> PdfText {
    pdf_text_within(bytes, MAX_PDF_TEXT_BYTES)
}

/// [`pdf_text`] with the aggregate budget supplied, so the bound is an offline test over a
/// handful of bytes rather than one that needs a multi-gigabyte fixture.
fn pdf_text_within(bytes: &[u8], budget: usize) -> PdfText {
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
        // ⚠ The aggregate bound, checked at the top so a document with more streams than any
        // reader will get through stops rather than being counted after the fact.
        // `MAX_INFLATED_BYTES` bounds *one* stream and a PDF holds as many as it likes.
        if collected.len() >= budget {
            break;
        }
        // `endstream` contains `stream`. Stepping over it here is cheaper and clearer than
        // a search that has to know about word boundaries.
        if at >= 3 && bytes.get(at - 3..at) == Some(b"end".as_slice()) {
            index = at + 6;
            continue;
        }

        let dictionary = dictionary_before(bytes, at);
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

        // Which bytes, if any, this stream contributes. Resolved into a value rather than
        // taken by a closure so the budget below can be applied in one place — a closure
        // that captured `collected` could not stop the loop it was called from.
        let mut inline: Option<&[u8]> = None;
        let mut inflated: Option<Vec<u8>> = None;

        if is_not_content(dictionary) {
            // A font, an image, a thumbnail, an object stream or the cross-reference table.
            // Skipped for cost rather than for correctness — `pdf_content_text` emits only
            // on a text-showing *operator*, which none of these contain, so the result would
            // be empty anyway. Cheap to skip a megabyte of JPEG.
        } else if find_bytes(dictionary, b"/Filter", 0).is_none() {
            inline = Some(body);
        } else if find_bytes(dictionary, b"FlateDecode", 0).is_some() {
            // ⚠ A `/Predictor` in `/DecodeParms` means the inflated bytes are PNG- or
            // TIFF-predicted and have to be un-predicted before they mean anything. That is
            // used for cross-reference and object streams rather than for page content, so
            // rather than emit scrambled text it is counted as unread and named.
            if find_bytes(dictionary, b"/Predictor", 0).is_some() {
                compressed += 1;
            } else {
                match inflate(body) {
                    Some(data) => inflated = Some(data),
                    None => compressed += 1,
                }
            }
        } else if find_bytes(dictionary, b"LZWDecode", 0).is_some() {
            // The one compression PDF still allows that `flate2` cannot do. Rare enough
            // since Acrobat 4 that implementing it would be work with no reader.
            compressed += 1;
        }

        if let Some(data) = inflated.as_deref().or(inline) {
            let text = pdf_content_text(data);
            if !text.trim().is_empty() {
                // One byte kept back for the separator, so the total cannot step past the
                // budget by the newline. Cut on a character boundary — this string is built
                // from arbitrary bytes in someone else's file and `panic = "abort"` has
                // ended this application twice on a byte index that was computed.
                let room = budget.saturating_sub(collected.len() + 1);
                collected.push_str(prefix_within(&text, room));
                collected.push('\n');
            }
        }
        index = body_end + 9;
    }

    let text = normalise_text(&collected);
    if !text.is_empty() {
        PdfText::Text(text)
    } else if compressed > 0 {
        PdfText::Unreadable { streams: compressed }
    } else {
        PdfText::NoText
    }
}

/// The dictionary belonging to the `stream` keyword at `at`.
///
/// **Found by balancing `>>` against `<<` backwards, not by taking the last `<<`.** A stream
/// dictionary very often contains a nested one — `/DecodeParms << /Predictor 12 >>` is the
/// common case, and it is precisely the one that decides whether the inflated bytes are text
/// or predicted nonsense. The last `<<` before the keyword belongs to *that* inner
/// dictionary, so the naive scan hands back a window with no `/Filter` in it and the stream
/// is then read as though it were uncompressed.
///
/// A/B'd rather than reasoned about: with the naive version, a `/Predictor` fixture answers
/// `NoText` — "this PDF has nothing in it" — where the truth is `Unreadable { streams: 1 }`.
///
/// An empty window for a stream with no dictionary at all, which is malformed but occurs.
fn dictionary_before(bytes: &[u8], at: usize) -> &[u8] {
    let mut depth = 0i32;
    let mut cursor = at;
    while cursor >= 2 {
        let Some(pair) = bytes.get(cursor - 2..cursor) else { break };
        if pair == b">>" {
            depth += 1;
            cursor -= 2;
        } else if pair == b"<<" {
            depth -= 1;
            if depth <= 0 {
                return bytes.get(cursor - 2..at).unwrap_or(&[]);
            }
            cursor -= 2;
        } else {
            cursor -= 1;
        }
    }
    &[]
}

/// Whether a stream's dictionary says it holds something other than page content.
///
/// A substring test over the dictionary rather than a parse, which is enough because these
/// markers do not occur in a content stream's own dictionary.
fn is_not_content(dictionary: &[u8]) -> bool {
    const NOT_CONTENT: [&[u8]; 6] =
        [b"/ObjStm", b"/XRef", b"/Metadata", b"/Image", b"/FontFile", b"/Thumb"];
    NOT_CONTENT.iter().any(|marker| find_bytes(dictionary, marker, 0).is_some())
}

/// The longest prefix of `text` that fits in `budget` **bytes**, cut on a character boundary.
///
/// `&text[..budget]` panics on any multi-byte character straddling the index, and with
/// `panic = "abort"` in the release profile that is the whole application. `CLAUDE.md`'s
/// feedback 30 records this exact shape aborting Velm twice, in a function whose own tests
/// used only ASCII.
fn prefix_within(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

/// Inflates a `FlateDecode`d stream, bounded by [`MAX_INFLATED_BYTES`].
///
/// Zlib first, then raw deflate. Both, because the specification says zlib and a
/// well-known population of real producers emits a raw deflate stream with no two-byte
/// header — a reader that only tries zlib reports those files as unreadable, which is
/// indistinguishable from the compression not being supported at all.
///
/// `None` for a stream that is neither: a damaged file, or one whose `/Length` was wrong and
/// took the body with it. Counted and named rather than treated as empty.
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    fn bounded<R: Read>(reader: R) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        // `take` before `read_to_end`, so a deflate bomb stops at the bound rather than
        // after it. Reading first and checking afterwards is not a bound.
        let mut reader = reader.take(MAX_INFLATED_BYTES as u64);
        match reader.read_to_end(&mut out) {
            // A truncated stream still yields the bytes that did inflate, and a page of text
            // that ends early is worth more than nothing at all.
            Ok(_) => Some(out),
            Err(_) if !out.is_empty() => Some(out),
            Err(_) => None,
        }
    }

    bounded(flate2::read::ZlibDecoder::new(data))
        .filter(|out| !out.is_empty())
        .or_else(|| bounded(flate2::read::DeflateDecoder::new(data)))
        .filter(|out| !out.is_empty())
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

/// A `.docx`, `.pptx` or `.xlsx`: a ZIP of XML, read in-process on every platform.
///
/// `textutil` is the fallback rather than the implementation, which is the reversal that
/// matters — it is macOS-only, so having it be the implementation meant Word documents
/// working on the primary target and nowhere else. It is still tried for a file this cannot
/// open, because a `.docx` that is really an old `.doc` with the wrong extension is a thing
/// that happens and `textutil` reads it.
fn office(source: &str, label: &str, path: &Path, kind: Kind, options: &Options) -> Ingested {
    office_within(source, label, path, kind, options, MAX_PART_BYTES, MAX_PARTS_BYTES)
}

/// [`office`] with both budgets supplied, for the same reason [`office_parts_within`] takes
/// them: what a file *past* the budget reports is the thing worth testing, and a fixture that
/// really is 64MB is not a test this machine should be asked to run sixty-five times over.
fn office_within(
    source: &str,
    label: &str,
    path: &Path,
    kind: Kind,
    options: &Options,
    per_part: usize,
    total: usize,
) -> Ingested {
    let read = match kind {
        Kind::Docx => docx_text(path, per_part, total),
        Kind::Pptx => pptx_text(path, per_part, total),
        Kind::Xlsx => xlsx_text(path, per_part, total),
        _ => None,
    };

    match read {
        Some((text, dropped)) if !text.trim().is_empty() => {
            let chars = text.chars().count();
            // ⚠ **A budget that left something behind is a `Partial`, never an `Extracted`.**
            // The reader's own caps are the only thing that can leave a hole here, and the hole
            // does not look like one: an `.xlsx` whose sheets spent the aggregate budget loses
            // `xl/sharedStrings.xml`, which is written last — so every text cell resolves to
            // the empty string and a workbook of words arrives as a grid of numbers, under
            // *"Read n characters"*. `Outcome::Partial` exists precisely so the node can say
            // which of the two it got, and its own doc comment is about this: a message that
            // covers all the reasons covers none of them.
            let outcome = if dropped {
                Outcome::Partial {
                    chars,
                    message: format!(
                        "{label} is larger than one attachment holds. {chars} characters were \
                         read and some of the file was left out, so words that are in it may \
                         be missing here."
                    ),
                }
            } else {
                Outcome::Extracted { chars }
            };
            Ingested::new(source, kind, label, text, outcome)
        }
        // It opened and there was nothing in it. Saying so beats offering a converter that
        // will also find nothing.
        Some(_) => Ingested::new(
            source,
            kind,
            label,
            String::new(),
            Outcome::Unsupported {
                message: format!("{label} opened, and there is no text in it."),
            },
        ),
        None => legacy_document(source, label, path, options),
    }
}

/// `.doc`, `.rtf`, `.odt`, `.pages` — and anything an Office reader could not open.
///
/// The one family left that needs an external converter. `textutil` reads all of them and
/// ships with macOS; elsewhere this is a named refusal, and the message says what to do
/// rather than what is missing.
fn legacy_document(source: &str, label: &str, path: &Path, options: &Options) -> Ingested {
    if options.tools.textutil {
        return match run_tool("textutil", &["-convert", "txt", "-stdout"], path, false) {
            Ok(raw) => {
                let text = normalise_text(&raw);
                let chars = text.chars().count();
                Ingested::new(source, Kind::LegacyDoc, label, text, Outcome::Extracted { chars })
            }
            Err(error) => Ingested::failed(
                source,
                Kind::LegacyDoc,
                label,
                format!("`textutil` could not read {label}: {error}"),
            ),
        };
    }

    Ingested::new(
        source,
        Kind::LegacyDoc,
        label,
        String::new(),
        Outcome::NeedsTool {
            tool: "textutil",
            message: format!(
                "Velm has no reader for {label}'s format. On macOS `textutil` does this and \
                 ships with the system; elsewhere, save it as `.docx`, `.txt` or `.md` — all \
                 three of those Velm reads by itself."
            ),
        },
    )
}

/// Word: one part, one line per paragraph.
///
/// Headers, footers, footnotes, endnotes and comments live in *other* parts and are
/// deliberately not read — they are page furniture, and folding a running header into the
/// prose once per page is worse than leaving it out.
///
/// The `bool` is whether the archive gave up everything it was asked for — see [`OfficeParts`].
/// All three readers carry it, not only the workbook one: a truncated `document.xml` is a
/// document missing its last pages, and the node must not call that a complete read.
fn docx_text(path: &Path, per_part: usize, total: usize) -> Option<(String, bool)> {
    let (parts, dropped) =
        office_parts_within(path, &|name| name == "word/document.xml", per_part, total)?;
    let (_, xml) = parts.into_iter().next()?;
    Some((
        normalise_text(&office_xml_text(
            &xml,
            &XmlText { words: "w:t", breaks: &["w:p"], newline: &["w:br", "w:cr"], tab: &["w:tab"] },
        )),
        dropped,
    ))
}

/// PowerPoint: one part per slide, **in slide order**.
///
/// The order is the reason this is not two lines. A ZIP lists its entries in whatever order
/// they were written, and the names sort lexically — so `slide10.xml` lands between
/// `slide1.xml` and `slide2.xml`, and a twelve-slide deck reaches the agent scrambled. Sorted
/// by the trailing number instead.
fn pptx_text(path: &Path, per_part: usize, total: usize) -> Option<(String, bool)> {
    let (mut parts, dropped) = office_parts_within(
        path,
        &|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"),
        per_part,
        total,
    )?;
    parts.sort_by_key(|(name, _)| slide_number(name).unwrap_or(u32::MAX));

    let mut out = String::new();
    for (index, (_, xml)) in parts.iter().enumerate() {
        let text = normalise_text(&office_xml_text(
            xml,
            &XmlText { words: "a:t", breaks: &["a:p"], newline: &["a:br"], tab: &[] },
        ));
        if text.is_empty() {
            continue;
        }
        // Numbered, because a deck read as continuous prose loses the one piece of structure
        // it has. The number is the slide's position in the deck, not its file name.
        out.push_str(&format!("Slide {}\n{text}\n\n", index + 1));
    }
    Some((out.trim().to_owned(), dropped))
}

/// The number in `ppt/slides/slide12.xml`.
fn slide_number(name: &str) -> Option<u32> {
    name.rsplit_once("slide")?.1.split('.').next()?.parse().ok()
}

/// Excel: every sheet, as tab-separated rows.
///
/// **Shared strings are the whole job.** A cell holding text does not hold the text: it holds
/// `t="s"` and an index into `xl/sharedStrings.xml`, so a reader that takes `<v>` at face
/// value produces a spreadsheet of integers where the words were. That is the failure this
/// resolves, and it is why `.xlsx` was worth doing rather than declaring.
///
/// **Declared limits**: a cell shows its stored value, so a date is the serial number Excel
/// stores (`45000`) rather than a date, and a currency is a bare number — number *formats*
/// are in `xl/styles.xml` and are not applied. A formula contributes its last cached result,
/// which is right, except in a workbook saved without one, where it contributes nothing.
fn xlsx_text(path: &Path, per_part: usize, total: usize) -> Option<(String, bool)> {
    let (parts, dropped) = office_parts_within(
        path,
        &|name| {
            name == "xl/sharedStrings.xml"
                || (name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
        },
        per_part,
        total,
    )?;

    let shared = parts
        .iter()
        .find(|(name, _)| name == "xl/sharedStrings.xml")
        .map(|(_, xml)| shared_strings(xml))
        .unwrap_or_default();

    let mut sheets: Vec<&(String, String)> =
        parts.iter().filter(|(name, _)| name != "xl/sharedStrings.xml").collect();
    // Same lexical-order trap as the slides, and the same fix.
    sheets.sort_by_key(|(name, _)| slide_number(name.trim_end_matches(".xml")).unwrap_or(u32::MAX));

    let mut out = String::new();
    for (_, xml) in sheets {
        for row in sheet_rows(xml, &shared) {
            out.push_str(&row);
            out.push('\n');
        }
    }
    Some((out.trim().to_owned(), dropped))
}

/// `xl/sharedStrings.xml` as a lookup table: one entry per `<si>`.
///
/// An `<si>` can hold several `<t>` runs when Excel has split a string by formatting, and
/// they are concatenated — taking only the first yields *"Torque"* where the cell says
/// *"Torque figures"*.
fn shared_strings(xml: &str) -> Vec<String> {
    let chars: Vec<char> = xml.chars().collect();
    let mut table = Vec::new();
    let mut current = String::new();
    let mut capture = false;
    let mut phonetic = false;
    let mut index = 0usize;

    while index < chars.len() {
        if chars[index] != '<' {
            if capture && !phonetic {
                current.push(chars[index]);
            }
            index += 1;
            continue;
        }
        let closing = chars.get(index + 1) == Some(&'/');
        let name = xml_tag_name(&chars, index);
        let after = skip_tag(&chars, index);
        match (closing, name.as_str()) {
            (false, "si") => current.clear(),
            (true, "si") => table.push(decode_entities(current.trim())),
            // `<rPh>` is the furigana Excel stores beside a Japanese string. It contains a
            // `<t>` of its own, and including it doubles every such cell.
            (false, "rPh") => phonetic = true,
            (true, "rPh") => phonetic = false,
            (false, "t") => capture = true,
            (true, "t") => capture = false,
            _ => {}
        }
        index = after;
    }
    table
}

/// One worksheet's rows, tab-separated, blank rows dropped.
fn sheet_rows(xml: &str, shared: &[String]) -> Vec<String> {
    let chars: Vec<char> = xml.chars().collect();
    let mut rows = Vec::new();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut value = String::new();
    let mut cell_type = String::new();
    let mut capture = false;
    let mut index = 0usize;

    while index < chars.len() {
        if chars[index] != '<' {
            if capture {
                value.push(chars[index]);
            }
            index += 1;
            continue;
        }
        let closing = chars.get(index + 1) == Some(&'/');
        let name = xml_tag_name(&chars, index);
        let after = skip_tag(&chars, index);
        let tag: String = chars.get(index..after).map(|s| s.iter().collect()).unwrap_or_default();

        match (closing, name.as_str()) {
            (false, "c") => {
                cell.clear();
                cell_type = xml_attr(&tag, "t").unwrap_or_default();
            }
            (false, "v" | "t") => {
                capture = true;
                value.clear();
            }
            (true, "v") => {
                capture = false;
                // `t="s"` is an *index*, not a value. This is the line that turns a
                // spreadsheet of integers back into the words the user typed.
                cell = if cell_type == "s" {
                    value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|at| shared.get(at))
                        .cloned()
                        .unwrap_or_default()
                } else {
                    decode_entities(value.trim())
                };
            }
            (true, "t") => {
                // An inline string: the text is here rather than in the shared table, and it
                // may arrive in several runs.
                capture = false;
                cell.push_str(&decode_entities(value.trim()));
            }
            (true, "c") => cells.push(std::mem::take(&mut cell)),
            (true, "row") => {
                if cells.iter().any(|cell| !cell.is_empty()) {
                    rows.push(cells.join("\t"));
                }
                cells.clear();
            }
            _ => {}
        }
        index = after;
    }
    rows
}

/// Which elements of an Office XML part carry words, and which end a line.
struct XmlText<'a> {
    /// The element whose character data is the text: `w:t` in Word, `a:t` in PowerPoint.
    words: &'a str,
    /// End tags that finish a line — the paragraph element.
    breaks: &'a [&'a str],
    /// Empty elements that insert a line break.
    newline: &'a [&'a str],
    /// Empty elements that insert a tab.
    tab: &'a [&'a str],
}

/// Text out of an Office XML part, respecting paragraph boundaries.
///
/// **Only the character data of `words` is taken**, rather than every text node with the tags
/// stripped. Office XML is full of elements whose content is not prose, and a generic strip
/// puts document settings and revision ids into the middle of a sentence. Paragraph
/// boundaries are the other half: without them a report is one run-on line, and an agent
/// given one paragraph of forty thousand characters has been given a worse document than the
/// one on disk.
fn office_xml_text(xml: &str, spec: &XmlText<'_>) -> String {
    let chars: Vec<char> = xml.chars().collect();
    let mut out = String::new();
    let mut depth = 0usize;
    let mut index = 0usize;

    while index < chars.len() {
        if chars[index] != '<' {
            if depth > 0 {
                out.push(chars[index]);
            }
            index += 1;
            continue;
        }
        let closing = chars.get(index + 1) == Some(&'/');
        let name = xml_tag_name(&chars, index);
        let after = skip_tag(&chars, index);
        // `<w:t/>` is legal and empty; counting it as an opening leaves the capture on for
        // the rest of the document.
        let empty = after.checked_sub(2).and_then(|at| chars.get(at)) == Some(&'/');

        if name == spec.words {
            if closing {
                depth = depth.saturating_sub(1);
            } else if !empty {
                depth += 1;
            }
        } else if (closing && spec.breaks.contains(&name.as_str()))
            || (!closing && spec.newline.contains(&name.as_str()))
        {
            // A paragraph *ending*, or a break element *starting*: both finish a line.
            out.push('\n');
        } else if !closing && spec.tab.contains(&name.as_str()) {
            out.push('\t');
        }
        index = after;
    }
    decode_entities(&out)
}

/// An XML tag's name: case preserved and the namespace kept, unlike [`tag_name`].
///
/// Both matter. XML is case-sensitive, and the prefix *is* part of the name here — `w:t` and
/// `a:t` are what tell a Word run from a PowerPoint one.
fn xml_tag_name(chars: &[char], at: usize) -> String {
    let mut index = at + 1;
    if chars.get(index) == Some(&'/') {
        index += 1;
    }
    let mut name = String::new();
    while let Some(ch) = chars.get(index) {
        if ch.is_ascii_alphanumeric() || *ch == ':' || *ch == '-' || *ch == '_' || *ch == '.' {
            name.push(*ch);
            index += 1;
        } else {
            break;
        }
    }
    name
}

/// One attribute out of a raw tag.
///
/// Matched with a leading space so an attribute whose name merely *ends* with the one being
/// asked for cannot answer — `<row ht="15">` must not satisfy a request for `t`.
fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)?;
    let rest = tag.get(at + key.len()..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

/// What came out of an archive, and **whether it is all of it**.
///
/// The flag is the whole reason this is not a bare `Vec`. A budget that quietly leaves parts
/// behind while the caller reports [`Outcome::Extracted`] is a lie about the file, and in the
/// shape that bites hardest it is not even a partial read: Excel writes `xl/sharedStrings.xml`
/// **after** the worksheets, and every text cell in the workbook is an *index* into it — so a
/// budget spent on sheets loses the string table and every one of those cells resolves to the
/// empty string. The words are gone, the row count is right, and the outcome said *"Read n
/// characters"*. Anything left out has to reach the caller.
type OfficeParts = (Vec<(String, String)>, bool);

/// Opens an Office file and reads every part the caller wants, within both budgets.
///
/// The one function in this module that touches the `zip` crate, so the archive is opened
/// once and the API surface stays to three calls. `None` means it is not a readable ZIP at
/// all — which is what sends [`office`] to the converter fallback.
///
/// Names are collected before any part is read because listing borrows the archive and
/// reading takes it mutably; there is no way to do both at once, and the alternative is
/// opening the file once per part.
///
/// The budgets are parameters rather than the constants directly, so the aggregate bound is an
/// offline test over an archive built in memory rather than one that needs a gigabyte of
/// fixture. [`office`] supplies [`MAX_PART_BYTES`] and [`MAX_PARTS_BYTES`].
fn office_parts_within(
    path: &Path,
    wanted: &dyn Fn(&str) -> bool,
    per_part: usize,
    total: usize,
) -> Option<OfficeParts> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let names: Vec<String> =
        archive.file_names().filter(|name| wanted(name)).map(str::to_owned).collect();

    let mut parts = Vec::new();
    let mut collected = 0usize;
    let mut dropped = false;
    for name in names {
        // ⚠ **The aggregate bound.** The per-part cap below passes on every entry of a ZIP
        // bomb — that is what makes it a bomb — while this function holds *all* of them at
        // once for the caller: a hundred sheets at `MAX_PART_BYTES` each is 3.2GB out of an
        // archive of a few hundred kilobytes.
        //
        // Nothing is left before the budget is actually spent, and that is the correction:
        // this used to `break` on the first entry that would cross the line, which threw away
        // **everything behind it**. Excel writes `xl/sharedStrings.xml` after the worksheets,
        // so a workbook whose sheets reach the cap lost the string table — and with it every
        // text cell in the file, since a text cell holds an index into it and not the words.
        // A `continue` reads the small parts behind a big one; a `break` cannot.
        let room = total.saturating_sub(collected);
        if room == 0 {
            dropped = true;
            break;
        }
        let Ok(entry) = archive.by_name(&name) else {
            // A wanted part the archive would not hand over is missing words too.
            dropped = true;
            continue;
        };
        let mut bytes = Vec::new();
        // Bounded: the compressed size on disk says nothing about the uncompressed size, and
        // a ZIP bomb is a hundred kilobytes that inflates without end. `take` before
        // `read_to_end`, so the cap is applied instead of the allocation rather than after it.
        //
        // The ceiling is the *smaller* of the two budgets and one byte over it, which does
        // two things: it tells an entry that exactly fills the remaining room from one that
        // overruns it, and it stops a bomb being inflated past what could ever be kept.
        // `saturating_add`, because a caller is free to pass `usize::MAX` as a budget — the
        // aggregate test does — and `+ 1` on that is an overflow panic in a debug build.
        let ceiling = per_part.min(room);
        if entry.take((ceiling as u64).saturating_add(1)).read_to_end(&mut bytes).is_err() {
            dropped = true;
            continue;
        }
        if bytes.len() > per_part {
            // Past the **per-part** cap: truncated, and reported. Half an XML part parses as a
            // shorter document rather than as a broken one, so most of a huge `document.xml`
            // beats none of it — but the caller is told, because "most" is not "all".
            bytes.truncate(per_part);
            dropped = true;
        } else if bytes.len() > room {
            // Within the per-part cap and too big for what is left of the **aggregate** one:
            // dropped whole, and the parts behind it are still read.
            dropped = true;
            continue;
        }
        collected += bytes.len();
        parts.push((name, String::from_utf8_lossy(&bytes).into_owned()));
    }
    Some((parts, dropped))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Builds a ZIP with **stored** (uncompressed) entries, by hand.
    ///
    /// Deliberately not the `zip` crate's writer. Two reasons, and the second is the one that
    /// matters: a fixture built by the same library that reads it cannot fail in the way a
    /// real Office file would, and the writer's option types have been renamed across `zip`
    /// releases — a fixture is not worth coupling the test to that. Stored entries also mean
    /// the file is readable by `unzip` and by Python's `zipfile`, which is how this generator
    /// was checked against something that is not itself.
    fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (n, slot) in table.iter_mut().enumerate() {
                let mut c = n as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
                *slot = c;
            }
            let mut crc = 0xFFFF_FFFFu32;
            for byte in data {
                crc = table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
            }
            crc ^ 0xFFFF_FFFF
        }

        let mut out: Vec<u8> = Vec::new();
        let mut directory: Vec<u8> = Vec::new();
        for (name, body) in entries {
            let offset = out.len() as u32;
            let data = body.as_bytes();
            let crc = crc32(data);
            let size = data.len() as u32;
            let name_len = name.len() as u16;

            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            directory.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            directory.extend_from_slice(&20u16.to_le_bytes()); // made by
            directory.extend_from_slice(&20u16.to_le_bytes()); // needed
            directory.extend_from_slice(&0u16.to_le_bytes()); // flags
            directory.extend_from_slice(&0u16.to_le_bytes()); // method
            directory.extend_from_slice(&0u16.to_le_bytes()); // time
            directory.extend_from_slice(&0u16.to_le_bytes()); // date
            directory.extend_from_slice(&crc.to_le_bytes());
            directory.extend_from_slice(&size.to_le_bytes());
            directory.extend_from_slice(&size.to_le_bytes());
            directory.extend_from_slice(&name_len.to_le_bytes());
            directory.extend_from_slice(&0u16.to_le_bytes()); // extra
            directory.extend_from_slice(&0u16.to_le_bytes()); // comment
            directory.extend_from_slice(&0u16.to_le_bytes()); // disk
            directory.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            directory.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }

        let cd_offset = out.len() as u32;
        let cd_size = directory.len() as u32;
        let count = entries.len() as u16;
        out.extend_from_slice(&directory);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with the directory
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out
    }

    /// Writes a fixture archive and hands back its path.
    fn office_file(dir: &Path, name: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, zip_of(entries)).unwrap();
        path
    }

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
        assert_eq!(classify("deck.pptx"), Kind::Pptx);
        assert_eq!(classify("book.xlsx"), Kind::Xlsx);
        assert_eq!(classify("macros.xlsm"), Kind::Xlsx, "a macro-enabled file is the same ZIP");
        // The split that decides whether an external converter is needed at all.
        assert_eq!(classify("old.doc"), Kind::LegacyDoc);
        assert_eq!(classify("notes.rtf"), Kind::LegacyDoc);
        assert_eq!(Kind::Pptx.tag(), "pptx");
        assert_eq!(Kind::LegacyDoc.tag(), "document");
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
        // ⚠ The message must **not** promise a transcription. Nothing consumes
        // `Outcome::Transcribe`, so the old wording — *"queued for transcription"* — described
        // a queue that does not exist, and the user would only find out by waiting. It says
        // what the agent actually gets instead.
        let said = handed.outcome.message();
        assert!(!said.contains("queued"), "the message promised a queue that does not exist: {said}");
        assert!(said.contains("path"), "the message did not say what the agent gets: {said}");
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

    /// **The test for the `flate2` dependency.** Producers have emitted compressed content
    /// streams by default for twenty years, so this is not an edge case — it is what a PDF
    /// is. Before the decompressor this exact file answered *"install pdftotext"*.
    ///
    /// The fixture is compressed with `flate2`'s own encoder, which is circular for zlib and
    /// not for anything being tested here: what is under test is finding the stream, reading
    /// its dictionary, inflating it and tokenising the result.
    #[test]
    fn a_flate_compressed_pdf_is_read_now_that_there_is_a_decompressor() {
        use std::io::Write as _;

        let content = b"BT /F1 12 Tf 72 720 Td (Torque figures.) Tj 0 -14 Td (Second line) Tj ET";
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content).unwrap();
        let squeezed = encoder.finish().unwrap();

        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n1 0 obj\n<< /Length ");
        pdf.extend_from_slice(squeezed.len().to_string().as_bytes());
        pdf.extend_from_slice(b" /Filter /FlateDecode >>\nstream\n");
        pdf.extend_from_slice(&squeezed);
        pdf.extend_from_slice(b"\nendstream\nendobj\ntrailer\n%%EOF\n");

        assert_eq!(
            pdf_text(&pdf),
            PdfText::Text("Torque figures.\nSecond line".into()),
            "a FlateDecode stream was not inflated"
        );

        // And end to end, with no external tool available at all — which is the whole point
        // of the dependency: this now works on a machine with no poppler and on Windows.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("spec.pdf");
        std::fs::write(&path, &pdf).unwrap();
        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        assert!(ingested.outcome.is_text(), "{:?}", ingested.outcome);
        assert!(ingested.text.contains("Torque figures."), "{:?}", ingested.text);
    }

    /// What is left after the decompressor: LZW, a `/Predictor`, and a stream too damaged to
    /// inflate. All three are counted and named rather than being read as empty — the failure
    /// that would otherwise look exactly like a PDF with nothing in it.
    #[test]
    fn a_stream_that_still_cannot_be_read_is_counted_and_named() {
        let lzw = b"%PDF-1.2\n1 0 obj\n<< /Length 6 /Filter /LZWDecode >>\nstream\n\
                    \x80\x0b\x60\x50\x22\x0c\nendstream\nendobj\n";
        assert_eq!(pdf_text(lzw), PdfText::Unreadable { streams: 1 });

        // Inflating this would succeed and produce predicted bytes, which are not text.
        // Emitting them would be worse than saying nothing.
        let predicted = b"%PDF-1.5\n1 0 obj\n<< /Length 9 /Filter /FlateDecode \
                          /DecodeParms << /Predictor 12 /Columns 4 >> >>\nstream\n\
                          \x78\x9c\x03\x00\x00\x00\x00\x01\nendstream\nendobj\n";
        assert_eq!(pdf_text(predicted), PdfText::Unreadable { streams: 1 });

        let damaged = b"%PDF-1.5\n1 0 obj\n<< /Length 8 /Filter /FlateDecode >>\nstream\n\
                        not deflate at all\nendstream\nendobj\n";
        assert_eq!(pdf_text(damaged), PdfText::Unreadable { streams: 1 });

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.pdf");
        std::fs::write(&path, lzw).unwrap();
        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        assert!(ingested.text.is_empty(), "a refusal must not pretend to have text");
        match &ingested.outcome {
            Outcome::NeedsTool { tool, message } => {
                assert_eq!(*tool, "pdftotext");
                assert!(message.contains("poppler"), "the message must say how to get it: {message}");
            }
            other => panic!("an unreadable stream did not name the tool: {other:?}"),
        }
    }

    /// A font and an image are streams too, and inflating them costs real time for a result
    /// that is always empty. Neither may contribute text, and — the assertion that matters —
    /// neither may be counted as *unreadable*, or every ordinary PDF would report a failure.
    #[test]
    fn a_font_or_an_image_stream_is_neither_read_nor_counted_against_the_file() {
        let pdf = b"%PDF-1.5\n\
                    1 0 obj\n<< /Type /XObject /Subtype /Image /Filter /DCTDecode >>\nstream\n\
                    \xff\xd8\xff\xe0 jpeg bytes\nendstream\nendobj\n\
                    2 0 obj\n<< /Type /ObjStm /Filter /FlateDecode >>\nstream\n\
                    not really deflate\nendstream\nendobj\n\
                    3 0 obj\n<< /Length 40 >>\nstream\nBT (Real content) Tj ET\nendstream\nendobj\n";
        assert_eq!(pdf_text(pdf), PdfText::Text("Real content".into()));
    }

    #[test]
    fn a_pdf_with_no_text_and_a_file_that_is_not_one_are_told_apart() {
        assert_eq!(pdf_text(b"not a pdf at all"), PdfText::NotPdf);
        assert_eq!(pdf_text(b"%PDF-1.4\n<< >>\nstream\n0 0 m 10 10 l S\nendstream\n"), PdfText::NoText);
    }

    /// **The test for the `zip` dependency**, and it runs with `Tools::none` on purpose:
    /// that is the assertion that a Word document no longer needs `textutil`, which is what
    /// makes `.docx` work on Windows. Before this, exactly this file answered *"install
    /// textutil"* on any machine that is not a Mac.
    #[test]
    fn a_word_document_is_read_with_no_converter_anywhere() {
        let temp = tempfile::tempdir().unwrap();
        let document = "<?xml version=\"1.0\"?><w:document><w:body>\
             <w:p><w:r><w:t>Torque figures for the swap.</w:t></w:r></w:p>\
             <w:p><w:r><w:t>Second </w:t></w:r><w:r><w:t>paragraph &amp; more.</w:t></w:r></w:p>\
             <w:sectPr><w:pgSz w:w=\"11906\"/></w:sectPr></w:body></w:document>";
        let path = office_file(
            temp.path(),
            "report.docx",
            &[("[Content_Types].xml", "<Types/>"), ("word/document.xml", document)],
        );

        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        assert!(ingested.outcome.is_text(), "{:?}", ingested.outcome);
        assert_eq!(ingested.source.kind, "docx");
        // Paragraphs on their own lines, runs joined within one, entities decoded — and
        // nothing from `<w:sectPr>`, which a generic tag-strip would have dragged in.
        assert_eq!(
            ingested.text,
            "Torque figures for the swap.\nSecond paragraph & more.",
            "{:?}",
            ingested.text
        );
    }

    /// A deck of twelve slides is the case that breaks a naive reader: ZIP entries sort
    /// lexically, so `slide10` lands between `slide1` and `slide2` and the agent is handed
    /// the deck out of order. The fixture is deliberately written in the wrong order too.
    #[test]
    fn a_slide_deck_is_read_in_slide_order_not_in_name_order() {
        let temp = tempfile::tempdir().unwrap();
        let slide = |text: &str| {
            format!(
                "<p:sld><p:cSld><p:spTree><p:sp><p:txBody>\
                 <a:p><a:r><a:t>{text}</a:t></a:r></a:p>\
                 </p:txBody></p:sp></p:spTree></p:cSld></p:sld>"
            )
        };
        let (one, two, ten) = (slide("First"), slide("Second"), slide("Tenth"));
        let path = office_file(
            temp.path(),
            "deck.pptx",
            &[
                ("ppt/slides/slide10.xml", ten.as_str()),
                ("ppt/slides/slide1.xml", one.as_str()),
                ("ppt/slides/slide2.xml", two.as_str()),
            ],
        );

        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        assert_eq!(ingested.source.kind, "pptx");
        let first = ingested.text.find("First").expect("slide 1 missing");
        let second = ingested.text.find("Second").expect("slide 2 missing");
        let tenth = ingested.text.find("Tenth").expect("slide 10 missing");
        assert!(first < second && second < tenth, "the deck came out unordered: {:?}", ingested.text);
        assert!(ingested.text.contains("Slide 1"), "slides are not labelled: {:?}", ingested.text);
    }

    /// The reason `.xlsx` was worth doing rather than declaring: a text cell holds an *index*
    /// into the shared-string table, so a reader that takes `<v>` at face value hands the
    /// agent a grid of integers. This asserts the words, and asserts the integers are gone.
    #[test]
    fn a_spreadsheet_resolves_its_shared_strings_rather_than_emitting_indices() {
        let temp = tempfile::tempdir().unwrap();
        let shared = "<sst><si><t>Part</t></si><si><t>Torque</t></si>\
                      <si><t>Rear </t><t>bush</t></si></sst>";
        let sheet = "<worksheet><sheetData>\
             <row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c></row>\
             <row r=\"2\"><c r=\"A2\" t=\"s\"><v>2</v></c><c r=\"B2\"><v>210</v></c></row>\
             <row r=\"3\"></row>\
             </sheetData></worksheet>";
        let path = office_file(
            temp.path(),
            "book.xlsx",
            &[("xl/sharedStrings.xml", shared), ("xl/worksheets/sheet1.xml", sheet)],
        );

        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        assert_eq!(ingested.source.kind, "xlsx");
        assert_eq!(
            ingested.text,
            "Part\tTorque\nRear bush\t210",
            "shared strings were not resolved: {:?}",
            ingested.text
        );
        // The empty row contributed nothing rather than a blank line.
        assert_eq!(ingested.text.lines().count(), 2);
    }

    /// A file that is not a readable ZIP falls through to the converter — a `.doc` renamed
    /// to `.docx` is a real thing — and with no converter the message says what to do.
    #[test]
    fn an_unopenable_office_file_falls_back_and_then_says_what_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.docx");
        std::fs::write(&path, b"\xd0\xcf\x11\xe0 an old OLE compound file").unwrap();

        let ingested = ingest_with(path.to_str().unwrap(), &offline());
        match &ingested.outcome {
            Outcome::NeedsTool { tool, message } => {
                assert_eq!(*tool, "textutil");
                assert!(message.contains(".docx"), "the message must offer a way out: {message}");
            }
            other => panic!("an unreadable office file did not name a converter: {other:?}"),
        }
    }

    /// The pure halves, on literals — where the parsing actually lives.
    #[test]
    fn the_office_xml_readers_respect_structure_rather_than_stripping_tags() {
        // Only `w:t` character data is words. A generic strip takes `Arial` and `Heading1`
        // out of the run properties and puts them in the middle of the sentence.
        let word = "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>\
                    <w:r><w:rPr><w:rFonts w:ascii=\"Arial\"/></w:rPr><w:t>Title</w:t></w:r></w:p>\
                    <w:p><w:r><w:t>A</w:t><w:tab/><w:t>B</w:t><w:br/><w:t>C</w:t></w:r></w:p>";
        let spec =
            XmlText { words: "w:t", breaks: &["w:p"], newline: &["w:br", "w:cr"], tab: &["w:tab"] };
        let text = normalise_text(&office_xml_text(word, &spec));
        assert_eq!(text, "Title\nA B\nC", "{text:?}");
        assert!(!text.contains("Arial") && !text.contains("Heading1"));

        // A self-closing `<w:t/>` must not leave the capture switched on for the rest of the
        // document, which is how one empty run swallows a whole file's markup.
        let empty = "<w:p><w:r><w:t/></w:r></w:p><w:p><w:pStyle w:val=\"X\"/><w:r><w:t>Kept</w:t></w:r></w:p>";
        assert_eq!(normalise_text(&office_xml_text(empty, &spec)), "Kept");

        // Shared strings: several runs in one `<si>` are one string, and furigana is not.
        let table = shared_strings(
            "<sst><si><t>Rear </t><t>bush</t></si>\
             <si><t>\u{6771}\u{4eac}</t><rPh><t>trash</t></rPh></si></sst>",
        );
        assert_eq!(table, vec!["Rear bush".to_owned(), "\u{6771}\u{4eac}".to_owned()]);

        // An inline string lives in the cell rather than the table.
        let inline = "<sheetData><row><c t=\"inlineStr\"><is><t>Inline</t></is></c>\
                      <c><v>42</v></c></row></sheetData>";
        assert_eq!(sheet_rows(inline, &[]), vec!["Inline\t42".to_owned()]);

        // `<row ht="15">` must not answer a request for the `t` attribute.
        assert_eq!(xml_attr("<c r=\"A1\" t=\"s\">", "t").as_deref(), Some("s"));
        assert_eq!(xml_attr("<row ht=\"15\">", "t"), None);
        assert_eq!(slide_number("ppt/slides/slide12.xml"), Some(12));
        assert_eq!(slide_number("ppt/slides/notes.xml"), None);
    }

    /// The shell-out, end to end, on a machine that has the converter — skipped elsewhere,
    /// exactly as the git tests in [`crate::worktree`] are. `run_tool` has no other coverage,
    /// and it is the whole implementation of the formats that are not ZIP-of-XML.
    ///
    /// Deliberately an `.rtf` rather than a `.docx`: `.docx` no longer goes anywhere near
    /// `textutil`, so pointing this at one would test the ZIP reader twice and the converter
    /// not at all. The fixture is built with the same tool that reads it, which is the only
    /// way to get a genuine `.rtf` into a test without committing a binary blob.
    #[test]
    fn a_legacy_document_is_read_when_the_converter_is_installed() {
        if !Tools::probe().textutil {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src.txt");
        let document = temp.path().join("notes.rtf");
        std::fs::write(&source, "Torque figures for the swap.\nSecond line.\n").unwrap();
        let made = Command::new("textutil")
            .args(["-convert", "rtf", "-output"])
            .arg(&document)
            .arg(&source)
            .status()
            .expect("textutil should run");
        assert!(made.success(), "the fixture could not be built");

        let ingested = ingest_with(document.to_str().unwrap(), &Options::default());
        assert!(ingested.outcome.is_text(), "{:?}", ingested.outcome);
        assert_eq!(ingested.source.kind, "document");
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

    /// ⚠ **A per-stream cap is not a cap on a document.** `MAX_INFLATED_BYTES` bounds one
    /// stream and `pdf_text` concatenated every stream in the file into one `String` with
    /// nothing counting the total — so ~128 objects, an unremarkable number, was 128 × 64MB
    /// ≈ 7.9GB in a process that also holds a GPU surface on an 8GB machine.
    ///
    /// Driven through the budget rather than the constant, so this is 200 bytes of fixture
    /// instead of a gigabyte of one. The assertion is the **total**, which is the thing that
    /// was unbounded: each individual stream here is well inside any per-item cap, which is
    /// exactly what makes it a bomb.
    #[test]
    fn a_pdf_with_many_streams_is_bounded_in_total_and_not_only_per_stream() {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        for index in 0..40 {
            pdf.extend_from_slice(b"<< >>\nstream\nBT (");
            pdf.extend_from_slice(format!("{index:04}").repeat(25).as_bytes());
            pdf.extend_from_slice(b") Tj ET\nendstream\n");
        }

        let unbounded = pdf_text_within(&pdf, usize::MAX);
        let PdfText::Text(whole) = unbounded else { panic!("the fixture produced no text") };
        assert!(whole.len() > 3_000, "the fixture is too small to test a budget: {}", whole.len());

        let bounded = pdf_text_within(&pdf, 512);
        let PdfText::Text(clipped) = bounded else { panic!("the budget produced no text") };
        assert!(
            clipped.len() <= 512,
            "the aggregate budget was not honoured: {} bytes",
            clipped.len()
        );
        // Bounded, not emptied: what did fit is still there and still readable.
        assert!(clipped.starts_with("0000"), "{}", &clipped[..20.min(clipped.len())]);

        // A budget cut on a byte index would abort the process on a multi-byte character —
        // this is `strip_site_affix`'s shape, which ended the application twice. A UTF-16BE
        // hex string, because that is how a PDF actually spells a non-Latin character: the
        // literal `(…)` form is decoded byte-wise as WinAnsi and never produces one.
        let cjk = format!(
            "%PDF-1.4\n<< >>\nstream\nBT <FEFF{}> Tj ET\nendstream\n",
            "4E2D".repeat(50)
        );
        // ⚠ **The budget has to be chosen so the cut lands *inside* a character, and the old
        // one did not.** `pdf_text_within` keeps a byte back for the separator, so the index
        // handed to `prefix_within` is `budget - 1`: at a budget of 100 that is 99, which is
        // 33 × 3 — an exact boundary on a three-byte character, where the walk has nothing to
        // do and a naive `&text[..room]` would have passed just as well. 101 makes the index
        // 100, which is one byte into the thirty-fourth character, so the walk is the only
        // thing standing between this and an abort.
        let PdfText::Text(text) = pdf_text_within(cjk.as_bytes(), 101) else { panic!("no text") };
        assert!(text.len() <= 101, "{} bytes", text.len());
        assert!(text.chars().all(|ch| ch == '\u{4e2d}'), "{text:?}");
        assert!(!text.is_empty(), "the boundary walk emptied the string instead of cutting it");
        // Cut **back** to the boundary rather than at the budget: 33 whole characters, 99
        // bytes. A cut at 100 is not representable as a `&str` at all.
        assert_eq!(text.len(), 99, "the cut did not land on a character boundary");
        assert_eq!(text.chars().count(), 33);
    }

    /// The same shape one layer down: `MAX_PART_BYTES` bounds one ZIP entry, and a `.pptx` or
    /// a `.xlsx` is *made* of entries — `office_parts_within` collects every matching one into a
    /// `Vec` before a caller sees any of them, so 100 sheets × 32MB ≈ 3.2GB out of an archive
    /// of a few hundred kilobytes. The per-entry cap passes on every entry, which is what
    /// makes it a bomb rather than a big file.
    ///
    /// ⚠ **The fixture is deliberately not twenty interchangeable payloads any more.** It was,
    /// and that is why it could not see the defect underneath the one it was written for: with
    /// every part the same size and the same worth, "the budget stopped after four" is
    /// indistinguishable from "the budget threw away the one part the file needed". A real
    /// `.xlsx` is not made of interchangeable parts — `xl/sharedStrings.xml` is written **last**
    /// and every text cell is an index into it — so the last small entry is named, is different,
    /// and is asserted for.
    #[test]
    fn every_part_of_an_office_file_is_bounded_in_total_and_not_only_per_part() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("book.xlsx");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        let payload = vec![b'a'; 1_000];
        for sheet in 1..=20 {
            writer.start_file(format!("xl/worksheets/sheet{sheet}.xml"), options).unwrap();
            writer.write_all(&payload).unwrap();
        }
        // Where Excel puts it: after the sheets, and small.
        writer.start_file("xl/sharedStrings.xml", options).unwrap();
        writer.write_all(&[b's'; 100]).unwrap();
        writer.finish().unwrap();

        let wanted = |name: &str| name.starts_with("xl/");

        // Unbounded in aggregate: every entry passes a generous per-part cap, and 21 of them
        // arrive at once. This is the arithmetic the bomb relies on.
        let (whole, dropped) = office_parts_within(&path, &wanted, 1_000_000, usize::MAX).unwrap();
        assert_eq!(whole.len(), 21);
        let total: usize = whole.iter().map(|(_, text)| text.len()).sum();
        assert_eq!(total, 20_100);
        assert!(!dropped, "a whole read reported that it had left something out");

        // With a total budget, the per-part cap is untouched and the *sum* is what stops.
        // The part that would cross the line is dropped whole rather than truncated: half an
        // XML part parses as a shorter document rather than as a broken one.
        let (capped, dropped) = office_parts_within(&path, &wanted, 1_000_000, 4_500).unwrap();
        let total: usize = capped.iter().map(|(_, text)| text.len()).sum();
        assert!(total <= 4_500, "the aggregate budget was not honoured: {total} bytes");
        assert!(!capped.is_empty(), "the budget refused everything rather than bounding it");
        assert!(capped.len() < 21, "nothing was left out, so nothing was bounded");
        assert!(
            capped.iter().all(|(_, text)| text.len() == 1_000 || text.len() == 100),
            "a part was truncated where it should have been dropped"
        );
        assert!(dropped, "parts were left out and the caller was not told");

        // ⚠ **And the string table behind them was still read.** This is the half a fixture of
        // interchangeable parts cannot assert: the budget is spent by the fourth sheet, and a
        // `break` there — which is what this used to do — takes every entry *behind* it,
        // including the one that holds the workbook's actual words. Every `t="s"` cell then
        // resolves to the empty string while the read still reports a number of characters.
        assert!(
            capped.iter().any(|(name, _)| name == "xl/sharedStrings.xml"),
            "the budget stopped at the first oversized part and lost the string table: {:?}",
            capped.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>()
        );

        // A per-part cap **truncates** and reports, where the aggregate one drops whole. Both
        // are honest; they are not the same answer, and the flag does not care which happened.
        let (short, dropped) = office_parts_within(&path, &wanted, 400, usize::MAX).unwrap();
        assert!(dropped, "a truncated part was reported as a complete read");
        assert!(
            short.iter().any(|(_, text)| text.len() == 400),
            "the per-part cap dropped a part it should have truncated"
        );
        assert_eq!(short.len(), 21, "the per-part cap lost a part instead of shortening it");
    }

    /// **A file the budget bit into is `Partial`, not `Extracted`.** The outcome is what the
    /// node shows and what a later reader believes; *"Read n characters"* over a workbook whose
    /// string table was left behind is a complete read of a file that arrived without its
    /// words. `Outcome::Partial` exists for exactly this and was not being reached.
    /// Through the budgets rather than through a 64MB fixture, which is `office_parts_within`'s
    /// own argument one layer up: what a file *past* the cap reports is the thing being tested,
    /// and the number the cap happens to hold is not.
    #[test]
    fn an_office_file_bigger_than_the_budget_says_it_was_only_partly_read() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        let options = zip::write::SimpleFileOptions::default();
        let sheet = |rows: &str| {
            format!("<worksheet><sheetData>{rows}</sheetData></worksheet>")
        };
        let row = |value: &str| {
            format!("<row><c t=\"s\"><v>{value}</v></c></row>")
        };

        // A workbook in the shape that loses its words: two fat sheets whose cells are
        // *indices*, and the string table they index — written last, as Excel writes it.
        let path = temp.path().join("book.xlsx");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let rows: String = (0..200).map(|_| row("0")).collect();
        let body = sheet(&rows);
        let table = "<sst><si><t>torque</t></si></sst>";
        for number in 1..=2 {
            writer.start_file(format!("xl/worksheets/sheet{number}.xml"), options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.start_file("xl/sharedStrings.xml", options).unwrap();
        writer.write_all(table.as_bytes()).unwrap();
        writer.finish().unwrap();

        // Room for one sheet and the table, and not for the second sheet — derived from the
        // fixture rather than written as a number, so editing a row above cannot quietly turn
        // this into a test of something else.
        let budget = body.len() + table.len() + 1;
        let read = office_within(
            "book.xlsx",
            "book.xlsx",
            &path,
            Kind::Xlsx,
            &Options::default(),
            1_000_000,
            budget,
        );
        assert!(
            matches!(read.outcome, Outcome::Partial { .. }),
            "a workbook the reader could not finish reported {:?}",
            read.outcome
        );
        assert!(
            read.text.contains("torque"),
            "the string table was skipped, so every text cell came back empty: {:?}",
            read.text
        );

        // And a file that fits is still a plain `Extracted` — a `Partial` on everything is the
        // same lie facing the other way, and it would make the distinction worthless.
        let read = office_within(
            "book.xlsx",
            "book.xlsx",
            &path,
            Kind::Xlsx,
            &Options::default(),
            MAX_PART_BYTES,
            MAX_PARTS_BYTES,
        );
        assert!(
            matches!(read.outcome, Outcome::Extracted { .. }),
            "a workbook that fitted reported {:?}",
            read.outcome
        );
    }

    #[test]
    fn whitespace_is_collapsed_the_way_a_reader_would() {
        assert_eq!(normalise_text("  a  \n\n\n\n  b  \n"), "a\n\nb");
        assert_eq!(normalise_text("\n\n\n"), "");
        assert_eq!(normalise_text("one\ttwo   three"), "one two three");
    }
}
