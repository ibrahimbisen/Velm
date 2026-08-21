//! Push-to-talk: **talking to one agent node** instead of typing into it.
//!
//! `docs/07-agent-canvas.md` §0.3 — *everything expensive is opt-in* — applied to the most
//! private input in the application. Feature 12 asks for voice *per node*, not for a global
//! voice channel: you hold a key over the agent you are addressing, and what you said becomes
//! that node's next prompt. There is no board-wide microphone and no "listening" state.
//!
//! # The policy this module enforces, rather than documents
//!
//! **Nothing listens without an explicit press.** No always-on stream, no wake word, no
//! capture at app start, no capture when a board opens. Three things make that structural
//! rather than a promise a caller has to keep:
//!
//! - [`microphone`] and [`PushToTalk::new`] **open no device**. The audio stream is created by
//!   [`PushToTalk::press`] and destroyed by [`PushToTalk::release`] or [`PushToTalk::cancel`];
//!   between gestures there is no stream, so there is nothing to leak and nothing to forget
//!   to close. Dropping a [`PushToTalk`] mid-press ends the stream with it.
//! - **`press` takes a [`VoiceTarget`], and a `VoiceTarget` can only be built from an
//!   [`AgentModel`] whose `voice` flag is on.** `AgentModel::voice` is off by default, so a
//!   node that has not opted in cannot be recorded against — the gate is in the type, not in
//!   an `if` at one call site that a second call site can forget.
//! - **A recording is bounded twice**: the capture's own buffer stops at
//!   [`MAX_UTTERANCE_SECONDS`] ([`max_samples`]), and [`PushToTalk::poll`] releases a press
//!   that has been held past it. Two guards because they fail differently — a caller that
//!   stops calling `poll` still cannot fill memory, and a capture implementation that ignores
//!   the cap is still cut off by the clock.
//!
//! # Two halves, one seam
//!
//! ```text
//!   press ──► VoiceCapture ──► Recording ──► Utterance ──► Transcribe ──► Dictation
//!            (a microphone,    (whatever    (16 kHz mono,  (a local      (plain text,
//!             or a fake)        the device   as a WAV)      binary, or    typed into
//!                               gave us)                    an API)       the node)
//! ```
//!
//! Everything above [`VoiceCapture`] is ordinary arithmetic over `i16`s and is tested with
//! [`CannedCapture`] — **no microphone is needed to test any of it**, which is the point of
//! the trait. The one implementation that needs a platform audio library lives behind
//! `#[cfg(feature = "voice")]`; without that feature the crate still compiles and
//! [`Unavailable`] answers [`NOT_BUILT_IN`], which names what to do rather than failing mute.
//!
//! # Transcription: local first, deliberately
//!
//! [`Preference::Auto`] is the default and prefers a **local** transcriber whenever one is
//! installed. A microphone is the most private input Velm has; quietly posting it to a third
//! party is not a default anybody would have chosen, and `docs/07` §0.3's "degrading to a
//! legible explanation" is what the hosted path is for — not what it is.
//!
//! # ⚠ The API key
//!
//! Resolved from [`crate::transport::http::Credentials`] at the moment a request is built, put
//! in one `authorization` header, and **never** anywhere else: not in [`Speech`] (which is a
//! serialisable setting and has no field for one), not in the multipart body, not in an error,
//! not in a `Debug`. The repository is public. `docs/07` §8a states this once so that no
//! module has to decide it for itself; this one is bound by it twice over, because the thing
//! being uploaded is a recording of the user's voice.
//!
//! # This module does not read the clock
//!
//! [`PushToTalk::press`], [`PushToTalk::release`] and [`PushToTalk::poll`] all take `now_ms`.
//! So "is a press that started at t held too long at t + 130s" is arithmetic in a unit test
//! rather than a two-minute wait, exactly as [`crate::schedule`] does it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::voice_pure::{NOT_BUILT_IN, Preference};

use crate::model::AgentModel;
use crate::provider::Provider;
#[cfg(feature = "native")]
use crate::transport::http::Credentials;
use crate::{AgentError, Result};

// =======================================================================================
// Constants
// =======================================================================================

/// The rate every transcriber here wants: 16 kHz mono.
///
/// whisper and every whisper-shaped model are trained at it, and the hosted endpoints
/// resample to it on arrival — so sending 48 kHz stereo is three times the upload for
/// bytes the far end throws away. Resampling once, here, is also what makes the WAV
/// handed to a local binary and the WAV posted to an API byte-identical.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// The longest one press may record.
///
/// Two minutes of 16 kHz mono is ~3.8 MB, which is a bounded, affordable mistake. This is a
/// **stuck-key bound, not a length preference**: a push-to-talk key that never comes up —
/// a lost focus, a crashed frame loop, a hand off the keyboard — must not be able to grow a
/// buffer until the machine gives out. See [`max_samples`] and [`PushToTalk::poll`].
pub const MAX_UTTERANCE_SECONDS: u64 = 120;

/// Below this peak amplitude a recording is silence rather than speech (~ -60 dBFS).
///
/// Not a refusal — [`Utterance::is_silent`] reports it and the caller decides — because a
/// quiet room and a muted microphone are indistinguishable from here, and refusing to send
/// a genuinely whispered sentence would be worse than a transcriber answering nothing.
pub const SILENCE_PEAK: i16 = 32;

/// Where a transcription setting is actually changed today.
///
/// ⚠ **This exists because six refusals in this file used to say "Preferences ▸ Voice", and
/// there is no such page.** Those strings became reachable the moment feature 12 was wired
/// into the app, and the likeliest real path reaches one of them — a user with whisper on
/// `PATH` and no model file named, which is what `Speech::default()` is. Sending somebody to
/// look for a settings page that does not exist costs them their time before it costs them
/// their trust, which is this repository's stated worst failure and the rule `docs/07` §14
/// ends on.
///
/// Named once so the day a Voice page is built there is one string to change and no chance of
/// four of the six being missed — which is feedback 35's sibling rule, applied before rather
/// than after.
pub const WHERE_TO_CONFIGURE: &str = "choose one in Preferences ▸ Voice";

/// Where the **hosted** half is configured, which is not that menu.
///
/// ⚠ Split from [`WHERE_TO_CONFIGURE`] deliberately, and the split is the honesty. Preferences
/// ▸ Voice covers the local path completely — which transcriber, and the model file it needs —
/// because that is the private one and the one that needs no key. A hosted transcriber
/// additionally needs a provider *and* a model name Velm refuses to guess
/// ([`Speech::hosted_model`] says why a pinned model id is wrong), so it has no rows there and
/// is set in the sidecar.
///
/// Pointing a hosted refusal at the menu would be this pair's own defect one page further in:
/// somebody opens Preferences ▸ Voice looking for the API settings the sentence promised, and
/// finds three rows and a file picker.
pub const WHERE_TO_CONFIGURE_HOSTED: &str =
    "set the `speech` key in library.json, beside your boards";

/// The local transcribers probed for when none is configured.
///
/// whisper.cpp's binary, under the two names it ships as. Deliberately short: a tool with a
/// different argument shape is supported by setting [`Speech::command`] **and**
/// [`Speech::args`], not by guessing which of them this list means.
pub const KNOWN_LOCAL_COMMANDS: [&str; 2] = ["whisper-cli", "whisper-cpp"];

/// The argument template used when none is configured, for whisper.cpp's `whisper-cli`.
///
/// `{model}` and `{audio}` are substituted by [`render_args`]. A template rather than a
/// hardcoded call because the *tool* is the user's choice: the one thing this module must
/// not do is make "a whisper-style binary you have configured" mean "this exact binary".
pub const DEFAULT_LOCAL_ARGS: [&str; 6] = ["-m", "{model}", "-f", "{audio}", "-nt", "-np"];

/// The path a transcription API is at, on every OpenAI-compatible server including
/// whisper.cpp's own and LM Studio's.
const TRANSCRIPTION_PATH: &str = "audio/transcriptions";

/// Named as ourselves, exactly as `vellum-link`'s and `transport/http.rs`'s are.
const USER_AGENT: &str = concat!("Velm/", env!("CARGO_PKG_VERSION"), " (agent canvas)");

/// A hosted transcription is an upload followed by a wait, and the wait scales with the
/// recording — but it is bounded, unlike a streamed chat answer, so a global timeout is
/// honest here where `transport/http.rs` deliberately refuses one.
const HOSTED_TIMEOUT: Duration = Duration::from_secs(180);

// =======================================================================================
// Half 1 — capture
// =======================================================================================

/// The most samples a recording may hold, for a given device format.
///
/// The cap the capture's own buffer applies, so that a press nobody releases costs a
/// bounded amount of memory whether or not anything is polling it.
pub const fn max_samples(sample_rate: u32, channels: u16) -> usize {
    (sample_rate as usize) * (channels as usize) * (MAX_UTTERANCE_SECONDS as usize)
}

/// What a device actually gave us: interleaved 16-bit PCM, at whatever rate and channel
/// count the hardware chose.
///
/// **Not** resampled and not downmixed — that is [`Recording::to_utterance`]'s job, and
/// keeping the two apart is what lets the resampler be tested against known input without
/// a device having to produce it.
///
/// Deliberately **no `Default`**: a default would carry a zero sample rate, and
/// [`Recording::duration_ms`] divides by it. [`Recording::new`] clamps; nothing else builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

impl Recording {
    /// Takes samples from a device, **truncated to the bound**.
    ///
    /// The truncation is here rather than at each call site so that no implementation of
    /// [`VoiceCapture`] — including one written later, including a fake — can hand back an
    /// unbounded recording. A capture that also stops filling its own buffer at
    /// [`max_samples`] is doing the same job earlier, which is where it costs nothing.
    pub fn new(mut samples: Vec<i16>, sample_rate: u32, channels: u16) -> Self {
        let rate = sample_rate.max(1);
        let channels = channels.max(1);
        samples.truncate(max_samples(rate, channels));
        Self { samples, sample_rate: rate, channels }
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// How long this recording is, in milliseconds of wall time.
    pub fn duration_ms(&self) -> u64 {
        let frames = self.samples.len() as u64 / u64::from(self.channels);
        frames * 1000 / u64::from(self.sample_rate)
    }

    /// Downmixes to mono and resamples to [`TARGET_SAMPLE_RATE`], tagged with the node it
    /// was recorded for.
    pub fn to_utterance(&self, node: impl Into<String>) -> Utterance {
        let mono = downmix(&self.samples, self.channels);
        let samples = resample(&mono, self.sample_rate, TARGET_SAMPLE_RATE);
        Utterance { node: node.into(), samples, sample_rate: TARGET_SAMPLE_RATE }
    }
}

/// Averages interleaved channels down to one.
///
/// Averaged rather than "take the left channel": a machine whose second channel is the one
/// carrying the microphone — an interface with the mic on input 2, which is ordinary — would
/// otherwise record silence and look like a broken feature.
fn downmix(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let channels = usize::from(channels);
    samples
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&sample| i32::from(sample)).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

/// Linear resampling between two rates.
///
/// Linear rather than windowed-sinc on purpose: the consumer is a speech model, the source
/// is a 48 kHz microphone being decimated to 16 kHz, and the aliasing a proper filter would
/// remove sits above 8 kHz where there is no speech energy that changes a transcript. A
/// resampler with a dependency and a filter design is a cost with no reader.
pub fn resample(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if samples.is_empty() || from_rate == 0 || to_rate == 0 {
        return Vec::new();
    }
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = f64::from(from_rate) / f64::from(to_rate);
    let out_len = ((samples.len() as f64) / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for index in 0..out_len {
        let position = index as f64 * ratio;
        let left = position.floor() as usize;
        // The last output sample can land exactly on — or a hair past — the last input
        // sample, and `left + 1` would index off the end. Holding the final value is the
        // right answer there; wrapping would fold the start of the recording onto its end.
        let right = (left + 1).min(samples.len() - 1);
        let fraction = position - position.floor();
        let a = f64::from(samples[left.min(samples.len() - 1)]);
        let b = f64::from(samples[right]);
        out.push((a + (b - a) * fraction).round() as i16);
    }
    out
}

/// What was said, ready to transcribe: 16 kHz mono PCM, and the node it belongs to.
///
/// The node id rides along from the press rather than being supplied again at the hand-off,
/// which is what makes it impossible to record against one agent and dictate into another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    node: String,
    samples: Vec<i16>,
    sample_rate: u32,
}

impl Utterance {
    /// Builds one directly. For tests and for a caller that already has 16 kHz mono.
    pub fn new(node: impl Into<String>, samples: Vec<i16>, sample_rate: u32) -> Self {
        Self { node: node.into(), samples, sample_rate: sample_rate.max(1) }
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn duration_ms(&self) -> u64 {
        self.samples.len() as u64 * 1000 / u64::from(self.sample_rate)
    }

    /// Whether nothing above the noise floor was recorded.
    ///
    /// Reported, not enforced — see [`SILENCE_PEAK`].
    pub fn is_silent(&self) -> bool {
        self.samples.iter().copied().map(i16::saturating_abs).max().unwrap_or(0) < SILENCE_PEAK
    }

    /// This utterance as a canonical 16-bit PCM WAV file.
    ///
    /// The one format both halves of transcription take: whisper.cpp reads it from a file and
    /// every OpenAI-compatible endpoint accepts it as an upload, so there is one encoder here
    /// rather than one per backend.
    pub fn wav(&self) -> Vec<u8> {
        wav_bytes(&self.samples, self.sample_rate)
    }
}

/// Encodes 16-bit mono PCM as a WAV file.
///
/// The 44-byte canonical header — `RIFF`/`WAVE`, a 16-byte `fmt ` chunk declaring PCM
/// (format 1), then `data`. Written by hand rather than with a crate: it is four integers and
/// two magic strings, all little-endian, and a dependency for it would be more code to audit
/// than the code it replaced.
///
/// ⚠ The two length fields are the ones that go wrong. `RIFF`'s size is **36 + data**, i.e.
/// everything after those first eight bytes; a writer that puts the whole file length there
/// produces a file most players still open, which is exactly why the mistake survives.
pub fn wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * u32::from(CHANNELS) * u32::from(BITS) / 8;
    let block_align = CHANNELS * BITS / 8;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // the fmt chunk's own length
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = uncompressed PCM
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Something that can record from a microphone.
///
/// # Not `Send`, and that is load-bearing
///
/// `cpal::Stream` is `!Send` on the Core Audio backend, so a capture that owns a live stream
/// cannot cross a thread boundary. Requiring `Send` here would make the real implementation
/// impossible to write. It costs nothing: a capture lives on the frame loop, where the press
/// and the release happen, and the only thing that crosses to a worker is the [`Utterance`],
/// which is plain data.
pub trait VoiceCapture {
    /// Opens the device and begins recording.
    ///
    /// **The only thing in this module that turns a microphone on.** Refuses when already
    /// recording rather than silently restarting, so a double press cannot discard the first
    /// half of a sentence.
    fn start(&mut self) -> Result<()>;

    /// Stops, closes the device, and hands back everything captured.
    fn stop(&mut self) -> Result<Recording>;

    /// Whether a device is open right now. Used to draw the node's recording state, so it
    /// must answer about the *device*, not about what the caller believes.
    fn is_recording(&self) -> bool;
}

/// ⚠ **Without this the module's own two halves cannot be joined.** [`microphone`] answers
/// `Box<dyn VoiceCapture>` — it has to, because which capture a build has is a `cfg` decision
/// — and [`PushToTalk::new`] takes a `C: VoiceCapture`. With no impl for the box, the only
/// constructor in this file and the only consumer of one in this file do not fit together,
/// and the whole press path is unreachable from any caller that did not pick a concrete
/// capture at compile time. Which is every real caller: the app cannot, since the feature may
/// be off.
///
/// Found by trying to write that caller. It is this repository's signature defect — code that
/// compiles, passes its own tests, and has no way to be called — surviving inside a module
/// whose tests all name a concrete type.
impl VoiceCapture for Box<dyn VoiceCapture> {
    fn start(&mut self) -> Result<()> {
        (**self).start()
    }

    fn stop(&mut self) -> Result<Recording> {
        (**self).stop()
    }

    fn is_recording(&self) -> bool {
        (**self).is_recording()
    }
}

/// The capture compiled into every build.
///
/// Not a stub that silently does nothing: it refuses by name with [`NOT_BUILT_IN`], which is
/// `docs/07` §0.3's *"degrading to a legible explanation rather than a dead button"* for the
/// one feature that cannot be built everywhere. It is also what keeps this crate's promise
/// that it compiles and tests on a machine with no audio stack at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unavailable;

impl VoiceCapture for Unavailable {
    fn start(&mut self) -> Result<()> {
        Err(AgentError::Refused(NOT_BUILT_IN.to_owned()))
    }

    fn stop(&mut self) -> Result<Recording> {
        Err(AgentError::Refused(NOT_BUILT_IN.to_owned()))
    }

    fn is_recording(&self) -> bool {
        false
    }
}

/// A capture that answers with samples it was given. **Test-only in spirit, public by need:**
/// `vellum-app`'s own fixtures have to drive a press-to-dictation gesture without a
/// microphone, exactly as `ingest::Tools::none` makes the degraded path reachable.
#[derive(Debug, Clone, Default)]
pub struct CannedCapture {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
    recording: bool,
    /// How many times a device was opened. The assertion behind *"nothing listens without a
    /// press"* — a count of zero after constructing everything is the whole claim.
    pub starts: u32,
}

impl CannedCapture {
    pub fn new(samples: Vec<i16>, sample_rate: u32, channels: u16) -> Self {
        Self {
            samples,
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            recording: false,
            starts: 0,
        }
    }

    /// A second of a 440 Hz tone at 16 kHz mono — something with a peak, so
    /// [`Utterance::is_silent`] answers `false` and a test is exercising speech-shaped input
    /// rather than a buffer of zeros that every stage happens to pass through.
    pub fn tone() -> Self {
        let samples = (0..TARGET_SAMPLE_RATE)
            .map(|index| {
                let phase = f64::from(index) / f64::from(TARGET_SAMPLE_RATE) * 440.0
                    * std::f64::consts::TAU;
                (phase.sin() * 8000.0) as i16
            })
            .collect();
        Self::new(samples, TARGET_SAMPLE_RATE, 1)
    }
}

impl VoiceCapture for CannedCapture {
    fn start(&mut self) -> Result<()> {
        if self.recording {
            return Err(AgentError::Refused("already recording".into()));
        }
        self.recording = true;
        self.starts += 1;
        Ok(())
    }

    fn stop(&mut self) -> Result<Recording> {
        if !self.recording {
            return Err(AgentError::Refused("nothing was being recorded".into()));
        }
        self.recording = false;
        Ok(Recording::new(self.samples.clone(), self.sample_rate, self.channels))
    }

    fn is_recording(&self) -> bool {
        self.recording
    }
}

// ---------------------------------------------------------------------------------------
// The real capture, behind `--features voice`
// ---------------------------------------------------------------------------------------

/// The microphone, on a build that has one.
///
/// Written against **cpal 0.18**, whose API differs from the long-lived 0.15 line in three
/// places this file touches: `SampleRate` and `ChannelCount` are plain type aliases rather
/// than tuple structs, `build_input_stream` takes its `StreamConfig` **by value**, and the
/// several `…Error` types were unified into one `cpal::Error`.
///
/// Constructing one opens nothing. [`VoiceCapture::start`] builds and plays the stream;
/// [`VoiceCapture::stop`] drops it, which is how cpal closes a device — so a press that ends
/// any way at all, including by this struct being dropped, releases the microphone.
#[cfg(feature = "voice")]
pub struct MicrophoneCapture {
    live: Option<Live>,
}

#[cfg(feature = "voice")]
struct Live {
    /// Held only to keep the device open — dropping it stops the stream.
    _stream: cpal::Stream,
    buffer: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
    sample_rate: u32,
    channels: u16,
}

#[cfg(feature = "voice")]
impl Default for MicrophoneCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "voice")]
impl MicrophoneCapture {
    /// **Touches no hardware.** See this module's policy note.
    pub const fn new() -> Self {
        Self { live: None }
    }
}

#[cfg(feature = "voice")]
impl VoiceCapture for MicrophoneCapture {
    fn start(&mut self) -> Result<()> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        if self.live.is_some() {
            return Err(AgentError::Refused("this node is already recording".into()));
        }

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            AgentError::Refused(
                "this machine has no microphone that Velm can see — check your input device in \
                 System Settings ▸ Sound"
                    .into(),
            )
        })?;
        let supported = device.default_input_config().map_err(|error| {
            AgentError::Refused(format!(
                "the microphone would not say what format it records in ({error}) — on macOS \
                 this is usually the microphone permission: allow Velm under System Settings ▸ \
                 Privacy & Security ▸ Microphone"
            ))
        })?;

        let format = supported.sample_format();
        // `StreamConfig` is `Copy` in cpal 0.18, so each arm below may take it by value.
        let config: cpal::StreamConfig = supported.config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;
        let cap = max_samples(sample_rate, channels);

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i16>::with_capacity(
            // A second of audio up front, so an ordinary utterance never reallocates on the
            // audio thread — where an allocation is a glitch, not a slowdown.
            (sample_rate as usize) * (channels as usize),
        )));

        // The callback runs on the audio thread. It does exactly two things — convert and
        // append, under a lock nothing else holds for long — and it **stops appending at the
        // cap** rather than growing until the press is noticed.
        macro_rules! stream_for {
            ($sample:ty, $convert:expr) => {{
                let sink = std::sync::Arc::clone(&buffer);
                device.build_input_stream(
                    config,
                    move |data: &[$sample], _: &cpal::InputCallbackInfo| {
                        let mut held = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if held.len() >= cap {
                            return;
                        }
                        let room = cap - held.len();
                        #[allow(clippy::redundant_closure_call)]
                        held.extend(data.iter().take(room).map(|&value| ($convert)(value)));
                    },
                    |error| log_stream_error(&error.to_string()),
                    None,
                )
            }};
        }

        let stream = match format {
            cpal::SampleFormat::F32 => {
                stream_for!(f32, |value: f32| (value.clamp(-1.0, 1.0) * 32767.0) as i16)
            }
            cpal::SampleFormat::I16 => stream_for!(i16, |value: i16| value),
            cpal::SampleFormat::U16 => {
                stream_for!(u16, |value: u16| (i32::from(value) - 32768) as i16)
            }
            cpal::SampleFormat::I32 => stream_for!(i32, |value: i32| (value >> 16) as i16),
            // `SampleFormat` is `#[non_exhaustive]`, and a device recording 24-bit or 64-bit
            // float is a real thing. Naming the format is the whole remedy — the user can
            // change it in Audio MIDI Setup — where a silent failure would read as a dead key.
            other => {
                return Err(AgentError::Refused(format!(
                    // `{other:?}` rather than `{other}`: `SampleFormat` is certain to derive
                    // `Debug` and is not certain to implement `Display`.
                    "this microphone records in a sample format Velm does not convert \
                     ({other:?}) — set the input to 16-bit or 32-bit float in Audio MIDI Setup"
                )));
            }
        };

        let stream = stream.map_err(|error| {
            AgentError::Refused(format!(
                "the microphone could not be opened ({error}) — on macOS, allow Velm under \
                 System Settings ▸ Privacy & Security ▸ Microphone"
            ))
        })?;
        stream.play().map_err(|error| {
            AgentError::Refused(format!("the microphone would not start recording ({error})"))
        })?;

        self.live = Some(Live { _stream: stream, buffer, sample_rate, channels });
        Ok(())
    }

    fn stop(&mut self) -> Result<Recording> {
        let Some(live) = self.live.take() else {
            return Err(AgentError::Refused(
                "nothing was being recorded — hold the microphone key to talk".into(),
            ));
        };
        // The stream is dropped with `live`, which is what closes the device. The samples are
        // taken *before* that drop would matter: the buffer is behind an `Arc`, so a callback
        // still in flight appends to a vector nobody reads again.
        let samples = std::mem::take(
            &mut *live.buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        Ok(Recording::new(samples, live.sample_rate, live.channels))
    }

    fn is_recording(&self) -> bool {
        self.live.is_some()
    }
}

/// An error reported by the audio thread.
///
/// Deliberately only logged: it arrives on a callback that must not block and cannot fail the
/// press, and the press already fails visibly if no samples arrive.
#[cfg(feature = "voice")]
fn log_stream_error(message: &str) {
    eprintln!("velm: the microphone stream reported an error: {message}");
}

/// The capture this build has.
///
/// One place the two are chosen between, so no caller decides for itself and a build without
/// the feature degrades in exactly one way. **Opens no device** — see the policy note.
///
/// Written as two `cfg`'d functions rather than one function with two `cfg`'d blocks: a
/// `#[cfg]`'d block in statement position has to evaluate to `()`, so the one-function form
/// does not compile with the feature on — which is the half nobody builds by accident.
#[cfg(feature = "voice")]
#[must_use]
pub fn microphone() -> Box<dyn VoiceCapture> {
    Box::new(MicrophoneCapture::new())
}

/// The capture this build has — see the other `cfg` of this function.
#[cfg(not(feature = "voice"))]
#[must_use]
pub fn microphone() -> Box<dyn VoiceCapture> {
    Box::new(Unavailable)
}

/// Whether this build can record at all. For greying a control with a reason rather than
/// offering one that always refuses.
#[must_use]
pub const fn capture_is_built_in() -> bool {
    cfg!(feature = "voice")
}

// ---------------------------------------------------------------------------------------
// The press
// ---------------------------------------------------------------------------------------

/// A node that has opted into voice.
///
/// **The gate, as a type.** [`PushToTalk::press`] takes one of these and nothing else, and
/// the only constructor reads [`AgentModel::voice`] — which is `false` unless the user turned
/// it on. So "a node that did not ask for voice cannot be recorded against" is enforced by
/// what compiles, not by an `if` that a second call site can forget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTarget {
    node: String,
}

impl VoiceTarget {
    /// `None` when this node has not enabled voice, or has no id.
    pub fn new(node: impl Into<String>, model: &AgentModel) -> Option<Self> {
        let node = node.into();
        (model.voice && !node.is_empty()).then_some(Self { node })
    }

    pub fn node(&self) -> &str {
        &self.node
    }
}

/// A press in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    node: String,
    since_ms: u64,
}

/// Push-to-talk over one capture.
///
/// Owns the whole gesture: which node is being addressed, when the press started, and the
/// bound on how long it may last. Holds no clock — every method that cares takes `now_ms`.
pub struct PushToTalk<C> {
    capture: C,
    held: Option<Held>,
}

impl<C: VoiceCapture> PushToTalk<C> {
    /// **Opens no device.** See this module's policy note.
    pub fn new(capture: C) -> Self {
        Self { capture, held: None }
    }

    /// The node currently being recorded for, if any.
    pub fn holding(&self) -> Option<&str> {
        self.held.as_ref().map(|held| held.node.as_str())
    }

    /// How long the press has been held. Zero when nothing is.
    pub fn held_ms(&self, now_ms: u64) -> u64 {
        self.held.as_ref().map_or(0, |held| now_ms.saturating_sub(held.since_ms))
    }

    /// Begins recording for one node.
    ///
    /// Refuses while another press is live rather than switching nodes mid-sentence: the
    /// first half of the utterance was addressed to somebody else, and splitting it between
    /// two agents is worse than refusing the second press.
    pub fn press(&mut self, target: &VoiceTarget, now_ms: u64) -> Result<()> {
        if let Some(held) = &self.held {
            return Err(AgentError::Refused(format!(
                "already recording for {} — let that key up first",
                held.node
            )));
        }
        self.capture.start()?;
        self.held = Some(Held { node: target.node.clone(), since_ms: now_ms });
        Ok(())
    }

    /// Ends the press and answers what was said.
    ///
    /// An empty recording is a refusal rather than an empty utterance: it means the device
    /// gave us nothing at all — a denied permission is the usual cause — and sending an empty
    /// WAV to a transcriber would turn that into *"the model heard nothing"*, which points at
    /// the wrong thing.
    pub fn release(&mut self, now_ms: u64) -> Result<Utterance> {
        let Some(held) = self.held.take() else {
            return Err(AgentError::Refused(
                "nothing was being recorded — hold the microphone key on an agent to talk to it"
                    .into(),
            ));
        };
        // Taken for symmetry with `press` and `poll` — the release itself is not timed, and a
        // parameter this module can be given the clock through is better than one it cannot.
        let _ = now_ms;
        let recording = self.capture.stop()?;
        if recording.is_empty() {
            return Err(AgentError::Refused(format!(
                "the microphone recorded nothing for {} — on macOS, check Velm under System \
                 Settings ▸ Privacy & Security ▸ Microphone",
                held.node
            )));
        }
        Ok(recording.to_utterance(held.node))
    }

    /// Abandons the press and throws the audio away.
    ///
    /// What Escape, a tab switch and losing the window do. Idempotent, and it stops the
    /// device even if the capture reports an error on the way — the device closing is the
    /// part that must not be conditional.
    pub fn cancel(&mut self) {
        self.held = None;
        let _ = self.capture.stop();
    }

    /// The clock half of the bound.
    ///
    /// Called once per frame. Answers `Some` exactly when a press has been held past
    /// [`MAX_UTTERANCE_SECONDS`] and has been released for it — so a stuck key becomes a
    /// finished utterance rather than a growing buffer. `None` on every other frame, which is
    /// almost all of them.
    pub fn poll(&mut self, now_ms: u64) -> Option<Result<Utterance>> {
        let held = self.held.as_ref()?;
        if now_ms.saturating_sub(held.since_ms) < MAX_UTTERANCE_SECONDS * 1000 {
            return None;
        }
        Some(self.release(now_ms))
    }

    /// The capture underneath, for a caller that needs to ask it something.
    pub fn capture(&self) -> &C {
        &self.capture
    }
}

// =======================================================================================
// The hand-off
// =======================================================================================

/// What voice produces: **text, and which node it was said to**.
///
/// Deliberately a plain value with no behaviour and no reference to a session. The app feeds
/// [`Dictation::text`] to the node exactly as if it had been typed, through whatever path a
/// typed prompt already takes — so voice adds no second way for a prompt to reach an agent,
/// and everything downstream (the undo group, the transcript, the rule cascade) is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dictation {
    pub node: String,
    pub text: String,
}

/// What a transcription worker reports back, drained once per frame like every other pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    /// The audio is on its way to a transcriber. Carries which one, so the node can say
    /// *"transcribing locally"* rather than leaving the user to guess whether their voice
    /// just left the machine.
    Transcribing { node: String, backend: String },
    /// It came back as words.
    Transcribed(Dictation),
    /// It did not. The message names what to do.
    Failed { node: String, message: String },
}

// =======================================================================================
// Half 2 — transcription
// =======================================================================================

/// Which of the two answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Local,
    Hosted,
}

/// The transcription setting, as it is stored.
///
/// **Carries no API key**, by construction: the key is resolved from
/// [`crate::transport::http::Credentials`] when a request is built. So this struct can be
/// serialised into the library sidecar, printed, logged and put in a bug report without
/// anybody having to think about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Speech {
    #[serde(skip_serializing_if = "is_default")]
    pub preference: Preference,

    /// The local transcriber's binary. `None` probes [`KNOWN_LOCAL_COMMANDS`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// The model file the local binary loads — whisper.cpp's `ggml-*.bin`. Required by every
    /// build of it, and there is nothing sensible to guess: the file is gigabytes and lives
    /// wherever the user put it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_file: Option<String>,

    /// The local binary's arguments, with `{model}` and `{audio}` substituted. Empty takes
    /// [`DEFAULT_LOCAL_ARGS`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Which provider a hosted transcription goes to. `None` is *not configured*, which is a
    /// different state from "configured and unreachable" and reports differently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted_provider: Option<Provider>,

    /// Overrides the provider's base URL. Required for a local server or a custom endpoint —
    /// a whisper.cpp server on this machine is reached through the *hosted* path with a
    /// `http://localhost:…/v1` base, which is the one arrangement that is both private and
    /// an API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted_base_url: Option<String>,

    /// The transcription model's name, verbatim as the provider spells it.
    ///
    /// **No default, and that is deliberate.** `transport/http.rs` resolves an unnamed chat
    /// model by asking the provider for its list and taking the newest; that cannot work here,
    /// because nothing in a model list says which entries transcribe audio, and picking the
    /// newest would send a recording to a chat model and get a 400 that reads like a bad key.
    /// A pinned constant is the other option and this repository has already decided against
    /// it: a model id written into a public source file is wrong within months and reads as a
    /// recommendation while it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted_model: Option<String>,
}

/// The `skip_serializing_if` `model.rs` uses, for the same reason: an unconfigured setting
/// writes nothing, so the sidecar stays small and diffable and a key that is not there cannot
/// be mistaken for one that is set to its default.
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

impl Speech {
    /// The local binary this setting names, if any is configured or installed.
    ///
    /// Probes `PATH` through [`crate::transport::probe_command`] — which looks for the file
    /// rather than running it, for the reason `ingest::Tools` records: a probe that executes
    /// a binary is a probe that can hang.
    #[cfg(feature = "native")]
    pub fn detect_local(&self) -> Option<String> {
        if let Some(command) = self.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            return crate::transport::probe_command(command).ok().map(|_| command.to_owned());
        }
        KNOWN_LOCAL_COMMANDS
            .iter()
            .copied()
            .find(|candidate| crate::transport::probe_command(candidate).is_ok())
            .map(str::to_owned)
    }

    /// Whether a hosted transcription has been configured at all.
    pub fn hosted_is_configured(&self) -> bool {
        self.hosted_provider.is_some() || self.hosted_base_url.is_some()
    }

    /// The transcriber to use, given what is installed.
    ///
    /// `local_available` is **passed in** rather than probed here so that "local is preferred
    /// when it is there" is an ordinary unit test on a machine that has no whisper installed —
    /// the `Tools::none` arrangement, applied again.
    pub fn transcriber(
        &self,
        data_dir: Option<&std::path::Path>,
        local_available: bool,
    ) -> Result<Box<dyn Transcribe>> {
        match choose_backend(self.preference, local_available, self.hosted_is_configured())? {
            Backend::Local => Ok(Box::new(self.local_transcriber()?)),
            Backend::Hosted => Ok(Box::new(self.hosted_transcriber(data_dir)?)),
        }
    }

    fn local_transcriber(&self) -> Result<LocalTranscriber> {
        let command = self.detect_local().ok_or_else(|| {
            AgentError::MissingCommand {
                command: self
                    .command
                    .clone()
                    .unwrap_or_else(|| KNOWN_LOCAL_COMMANDS[0].to_owned()),
            }
        })?;
        let args = if self.args.is_empty() {
            DEFAULT_LOCAL_ARGS.iter().map(|arg| (*arg).to_owned()).collect()
        } else {
            self.args.clone()
        };
        Ok(LocalTranscriber { command, model_file: self.model_file.clone(), args })
    }

    fn hosted_transcriber(&self, data_dir: Option<&std::path::Path>) -> Result<HostedTranscriber> {
        let provider = self.hosted_provider.unwrap_or(Provider::Custom);

        // Neither of these has a transcription endpoint of any shape Velm speaks. Named as a
        // gap, exactly as `transport/http.rs` names Gemini's chat API — pointing them at the
        // OpenAI path would 404 every request and read as a refused key.
        if matches!(provider, Provider::Claude | Provider::Gemini) {
            return Err(AgentError::Refused(format!(
                "{} has no speech-to-text endpoint Velm can use — install a whisper binary and \
                 let Velm transcribe on this machine, or name a provider that transcribes ({}).",
                provider.label(),
                WHERE_TO_CONFIGURE_HOSTED
            )));
        }

        let base = self
            .hosted_base_url
            .as_deref()
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .or_else(|| provider.default_base_url())
            .ok_or_else(|| {
                AgentError::Refused(
                    "there is no address to send the recording to — set the transcription base \
                     URL (an OpenAI-compatible one, e.g. http://localhost:8080/v1 for a \
                     whisper.cpp server)"
                        .into(),
                )
            })?
            .trim_end_matches('/')
            .to_owned();

        let model = self
            .hosted_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| {
                AgentError::Refused(
                    "name the transcription model this endpoint should use — Velm does not \
                     guess one, because a model list does not say which of its entries \
                     transcribe audio and a wrong guess reads as a refused key"
                        .into(),
                )
            })?
            .to_owned();

        let key = if provider.needs_api_key() {
            let key = data_dir
                .and_then(|directory| Credentials::load(directory).ok())
                .and_then(|credentials| credentials.key_for(provider));
            if key.is_none() {
                return Err(AgentError::Unauthorized {
                    provider: provider.label().to_owned(),
                    message: "there is no API key for this provider — sign in to it on an \
                              agent node, or transcribe on this machine instead"
                        .into(),
                });
            }
            key
        } else {
            None
        };

        Ok(HostedTranscriber { provider, base, model, key })
    }
}

/// Which backend a preference resolves to, given what exists.
///
/// Pure, and the availability comes in as arguments — see [`Speech::transcriber`].
///
/// ⚠ The `Auto` arm's order **is** the privacy policy: local first whenever it is installed.
/// Reversing it would be a one-word change that silently starts uploading the user's voice.
pub fn choose_backend(
    preference: Preference,
    local_available: bool,
    hosted_configured: bool,
) -> Result<Backend> {
    match preference {
        Preference::Local => {
            if local_available {
                Ok(Backend::Local)
            } else {
                Err(AgentError::Refused(format!(
                    "voice is set to transcribe only on this machine, and no whisper binary was \
                     found — install one (its command is usually `{}`) and name its model \
                     file, or allow the API instead. {}",
                    KNOWN_LOCAL_COMMANDS[0],
                    WHERE_TO_CONFIGURE
                )))
            }
        }
        Preference::Hosted => {
            if hosted_configured {
                Ok(Backend::Hosted)
            } else {
                Err(AgentError::Refused(format!(
                    "voice is set to transcribe through an API, and none is configured — name \
                     a transcription provider and model ({WHERE_TO_CONFIGURE_HOSTED})"
                )))
            }
        }
        // Local first. Not a tie-break: a microphone is the most private input in the
        // application and the default must not be the one that leaves the machine.
        Preference::Auto if local_available => Ok(Backend::Local),
        Preference::Auto if hosted_configured => Ok(Backend::Hosted),
        Preference::Auto => Err(AgentError::Refused(format!(
            "there is no way to turn speech into text yet — either install a whisper binary on \
             this machine (its command is usually `{}`, and it needs a model file: {}) or name \
             a transcription provider and model ({}). Nothing was recorded to anywhere in the \
             meantime",
            KNOWN_LOCAL_COMMANDS[0],
            WHERE_TO_CONFIGURE,
            WHERE_TO_CONFIGURE_HOSTED
        ))),
    }
}

/// Turning an utterance into words.
///
/// A trait so the seam is testable: [`CannedTranscriber`] answers a fixed string, which is
/// what lets the whole press-to-dictation path be exercised with neither a microphone nor a
/// network. `Send`, because a transcription runs on a worker thread — the shape `links.rs`
/// and every transport in this crate already use.
pub trait Transcribe: Send {
    fn transcribe(&self, utterance: &Utterance) -> Result<String>;

    /// Which backend this is.
    fn backend(&self) -> Backend;

    /// What the node says while this one is working — *"whisper.cpp, on this machine"*. It
    /// names where the audio went, which is the one thing worth showing during the wait.
    fn label(&self) -> String;
}

/// A transcriber that answers what it was given.
#[derive(Debug, Clone)]
pub struct CannedTranscriber {
    pub text: String,
}

impl CannedTranscriber {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl Transcribe for CannedTranscriber {
    fn transcribe(&self, _utterance: &Utterance) -> Result<String> {
        Ok(self.text.clone())
    }

    fn backend(&self) -> Backend {
        Backend::Local
    }

    fn label(&self) -> String {
        "a canned transcriber".to_owned()
    }
}

// ---------------------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------------------

/// A whisper-style binary the user has installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTranscriber {
    pub command: String,
    pub model_file: Option<String>,
    pub args: Vec<String>,
}

impl LocalTranscriber {
    /// Runs the tool over one audio file already on disk.
    ///
    /// Factored out of [`Transcribe::transcribe`] so that **a recording and a media file take
    /// the same path** — feature 18's last step is this function with a `.m4a` instead of a
    /// temporary `.wav`. A second copy of the argument rendering and the stderr handling would
    /// be two ideas of what "the transcriber refused" means, and the two would drift.
    pub fn run_on(&self, audio: &std::path::Path) -> Result<String> {
        let args = render_args(&self.args, self.model_file.as_deref(), audio)?;

        let output = Command::new(&self.command).args(&args).output().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AgentError::MissingCommand { command: self.command.clone() }
            } else {
                AgentError::Refused(format!("`{}` would not run: {error}", self.command))
            }
        })?;

        if !output.status.success() {
            // The tool's own complaint is the only useful thing to show, and it names the
            // missing model file — by far the commonest way this fails. Truncated by
            // **characters**: a byte slice panics on any multi-byte character straddling the
            // boundary, and the release profile aborts on a panic (feedback 30).
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail: String = stderr.trim().chars().take(400).collect();
            return Err(AgentError::Refused(if detail.is_empty() {
                format!("`{}` could not transcribe that recording", self.command)
            } else {
                format!("`{}`: {detail}", self.command)
            }));
        }

        Ok(clean_transcript(&String::from_utf8_lossy(&output.stdout)))
    }
}

impl Transcribe for LocalTranscriber {
    fn transcribe(&self, utterance: &Utterance) -> Result<String> {
        let wav = TempWav::write(&utterance.wav())?;
        self.run_on(&wav.path)
    }

    fn backend(&self) -> Backend {
        Backend::Local
    }

    fn label(&self) -> String {
        format!("`{}`, on this machine", self.command)
    }
}

/// Turn a media **file** into text — feature 18's missing last step.
///
/// `ingest` produces a [`crate::ingest::MediaHandoff`] and deliberately extracts nothing from
/// audio or video; its own doc names the contract — *"whoever owns transcription reads this
/// hand-off, produces the text"* — and until now nobody did, so a dropped recording reached
/// the agent as a filename.
///
/// # Local only, and that is the design rather than a shortcut
///
/// A hosted transcriber would take the file whole and is **refused here**. Sending somebody's
/// video to an API because they dragged it onto a node is a decision they did not make: a
/// spoken prompt is a thing the user just said into a microphone knowing they were talking to
/// an agent, and a file on their disk is not. The local path leaves nothing, so it needs no
/// such consent — which is why this is the half that ships.
///
/// The refusal names the remedy, so a user with no whisper installed learns why the file was
/// attached by name rather than discovering it by reading the agent's confused reply.
pub fn transcribe_media(speech: &Speech, audio: &std::path::Path) -> Result<String> {
    let Some(command) = speech.detect_local() else {
        return Err(AgentError::Refused(format!(
            "Velm transcribes media only with a transcriber on this machine, and none is \
             installed — its command is usually `{}`. Install one and {WHERE_TO_CONFIGURE}; \
             until then the agent is given the file's path rather than its words. An API is \
             deliberately not used here: that would upload your file.",
            KNOWN_LOCAL_COMMANDS[0]
        )));
    };
    LocalTranscriber {
        command,
        model_file: speech.model_file.clone(),
        args: if speech.args.is_empty() {
            DEFAULT_LOCAL_ARGS.iter().map(|argument| (*argument).to_owned()).collect()
        } else {
            speech.args.clone()
        },
    }
    .run_on(audio)
}

/// Substitutes `{model}` and `{audio}` into an argument template.
///
/// Pure, so the exact command line a tool is invoked with is a unit test rather than
/// something only observable by watching a process list. An `{audio}` the template forgot is
/// **appended**, since a transcriber with no audio argument is a call that cannot work; a
/// `{model}` with nothing to put in it is a refusal, because substituting an empty string
/// gives whisper.cpp a `-m` with no value and an error nobody can act on.
pub fn render_args(
    template: &[String],
    model_file: Option<&str>,
    audio: &std::path::Path,
) -> Result<Vec<String>> {
    let audio = audio.to_string_lossy().into_owned();
    let mut rendered = Vec::with_capacity(template.len() + 1);
    let mut saw_audio = false;

    for argument in template {
        if argument.contains("{model}") {
            let model = model_file.map(str::trim).filter(|model| !model.is_empty()).ok_or_else(
                || {
                    AgentError::Refused(
                        format!(
                            "the local transcriber needs a whisper model file (a \
                             `ggml-*.bin`) — name one, or transcribe through an API instead. \
                             {WHERE_TO_CONFIGURE}"
                        ),
                    )
                },
            )?;
            rendered.push(argument.replace("{model}", model));
            continue;
        }
        if argument.contains("{audio}") {
            saw_audio = true;
            rendered.push(argument.replace("{audio}", &audio));
            continue;
        }
        rendered.push(argument.clone());
    }

    if !saw_audio {
        rendered.push(audio);
    }
    Ok(rendered)
}

/// Turns a whisper binary's stdout into one line of speech.
///
/// Three shapes have to survive this, and every one of them is real output from some build of
/// whisper.cpp: a bare sentence, a set of lines each prefixed with a `[00:00:00.000 --> …]`
/// timestamp (which `-nt` suppresses and not every build honours), and a recording of silence,
/// which comes back as the literal tag `[BLANK_AUDIO]`. A bracketed tag on its own line is
/// **not** speech and must not become the agent's prompt.
pub fn clean_transcript(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut line = line.trim();
        // A leading `[…]` is a timestamp or a tag. Strip **every** one of them, not just the
        // first: a silent segment comes back as `[00:00:00.000 --> 00:00:02.000] [BLANK_AUDIO]`
        // — a timestamp *and* a tag — so stripping one leaves the other standing, and
        // `[BLANK_AUDIO]` becomes the agent's prompt. A turn nobody asked for, started by
        // saying nothing.
        while let Some(rest) = line.strip_prefix('[')
            && let Some((_tag, after)) = rest.split_once(']')
        {
            line = after.trim();
        }
        if line.is_empty() {
            continue;
        }
        parts.push(line.to_owned());
    }
    // Joined with a space rather than a newline: this becomes a prompt, and a transcript
    // broken across lines by whisper's own segmentation is one sentence, not several.
    parts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A WAV on disk that removes itself.
///
/// A local binary reads a *file*; whisper.cpp has no usable stdin path. `tempfile` is a
/// dev-dependency and this runs in a shipped build, so the name is built from the process id
/// and a counter — **not the clock**, which this crate does not read. The `Drop` is what
/// matters: the failure paths above return early, and a recording of the user's voice must
/// not be what gets left behind in `/tmp`.
struct TempWav {
    path: PathBuf,
}

impl TempWav {
    fn write(bytes: &[u8]) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "velm-voice-{}-{}.wav",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);

        // Mode 0600 for the same reason `credentials.json` is: it is a recording of somebody
        // talking, on a machine that may have other accounts on it.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
        std::io::Write::write_all(&mut file, bytes)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
        Ok(Self { path })
    }
}

impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------------------
// Hosted
// ---------------------------------------------------------------------------------------

/// A transcription API — including one running on this machine.
///
/// **No `Debug`**, deliberately, and for the reason `transport/http.rs`'s `Config` has none:
/// this is the one struct in the module that holds a key, and a derived `Debug` puts it into
/// the first `dbg!` anybody writes and into any panic message that formats a struct holding
/// one. The repository is public.
pub struct HostedTranscriber {
    provider: Provider,
    base: String,
    model: String,
    key: Option<String>,
}

#[cfg(feature = "native")]
impl Transcribe for HostedTranscriber {
    fn transcribe(&self, utterance: &Utterance) -> Result<String> {
        let wav = utterance.wav();
        let boundary = unique_boundary(&wav);
        let body = multipart_body(&boundary, &self.model, &wav);

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            // A non-2xx is a *response*: its body carries the provider's own explanation,
            // which is the only useful thing to put in front of the user.
            .http_status_as_error(false)
            .timeout_global(Some(HOSTED_TIMEOUT))
            .build()
            .into();

        let mut request = agent
            .post(format!("{}/{TRANSCRIPTION_PATH}", self.base))
            .header("content-type", format!("multipart/form-data; boundary={boundary}"));
        if let Some(key) = self.key.as_deref() {
            // The one place the key appears. Not in the body, not in the URL, not in a log.
            request = request.header("authorization", format!("Bearer {key}"));
        }

        let mut response = request
            .send(&body[..])
            .map_err(|error| AgentError::Transport {
                transport: "http",
                message: error.to_string(),
            })?;

        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(hosted_error(self.provider, status, &text));
        }
        read_transcription(&text)
    }

    fn backend(&self) -> Backend {
        Backend::Hosted
    }

    fn label(&self) -> String {
        format!("{} · {}", self.provider.label(), self.model)
    }
}

/// A `multipart/form-data` body carrying the model name and the WAV.
///
/// Written by hand, like the WAV header: it is two field headers and a terminator, and the
/// shape is fixed by RFC 7578. The parts that go wrong are the `\r\n`s — every line ending
/// here is CRLF, including the blank line before each part's content — and the closing
/// delimiter, which is `--boundary--` with **two** trailing hyphens.
///
/// ⚠ **The key is never a form field.** It goes in the `authorization` header and nowhere
/// else, so a body captured by a proxy, a test, or a bug report carries no secret.
pub fn multipart_body(boundary: &str, model: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    field("model", model);
    // JSON rather than `text`, and this field is now load-bearing rather than a preference:
    // `read_transcription` requires the `{"text": …}` shape, because its old fallback to the
    // raw body turned a captive portal's login page into the agent's next prompt. Every
    // OpenAI-compatible server answers this format with that shape, whisper.cpp's included.
    field("response_format", "json");

    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// A boundary that does not occur in the payload.
///
/// PCM is arbitrary bytes, so "a boundary that cannot appear in the body" is not something a
/// prefix can promise — and a boundary that *does* appear truncates the upload at that point
/// and produces a transcript of the first half of the sentence, which is a bug nobody would
/// diagnose. One scan and a suffix is the whole cost.
fn unique_boundary(payload: &[u8]) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = format!("velmvoice{}{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    dodge_boundary(base, payload)
}

/// The retry loop of [`unique_boundary`], with the starting name supplied.
///
/// Split out **so the loop can actually be tested**. The assertion that used to guard it fed
/// `unique_boundary` a payload holding the boundary's *prefix* — but the generated name is
/// longer than that prefix, so `contains` returned `false` on its length check before it
/// compared a byte, and the loop below never ran once in the test suite's life. The counter
/// makes the name unpredictable from outside, so there is no payload a caller of
/// `unique_boundary` can build that is guaranteed to collide: the seam has to be here.
fn dodge_boundary(base: String, payload: &[u8]) -> String {
    let mut boundary = base;
    for attempt in 0..16 {
        if !contains(payload, boundary.as_bytes()) {
            break;
        }
        boundary.push_str(&format!("x{attempt}"));
    }
    boundary
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|window| window == needle)
}

/// The text out of a transcription response: `{"text": "…"}`, and **nothing else**.
///
/// ⚠ **This used to fall back to the whole body, and what that produced was the agent's next
/// prompt.** A 200 is not a transcription — a captive portal answers 200 with a login page, a
/// proxy answers 200 with an error document, a misconfigured server answers 200 with its own
/// index — and every one of those became, verbatim and uncapped, *what the user said*. There
/// is nothing on screen to disbelieve at that point: the words are in the prompt box, so they
/// look like a bad transcription rather than like no transcription at all.
///
/// The fallback existed for a server that honoured a `text` response format. It cannot arise:
/// [`multipart_body`] asks for `response_format=json`, and every OpenAI-compatible server —
/// whisper.cpp's included — answers that with the documented shape.
///
/// A failure names the provider's own words when it gave any, and otherwise the *shape* of
/// what came back, capped by **characters** because a body can be a megabyte of HTML and
/// `&body[..200]` aborts the process on any multi-byte character straddling the boundary
/// (`CLAUDE.md` feedback 30, twice, with `panic = "abort"` in the release profile).
fn read_transcription(body: &str) -> Result<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(text) = value["text"].as_str() {
            return Ok(text.trim().to_owned());
        }
        // A JSON error body: the provider named the problem, so quote it rather than the
        // shape. Both spellings, because the bare-string form is what local servers send.
        let named = value["error"]["message"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .or_else(|| value["message"].as_str());
        if let Some(message) = named {
            return Err(AgentError::Transport {
                transport: "http",
                message: format!(
                    "the transcription endpoint answered 200 and then reported: {}",
                    message.trim().chars().take(200).collect::<String>()
                ),
            });
        }
    }
    let excerpt: String = body.trim().chars().take(200).collect();
    Err(AgentError::Transport {
        transport: "http",
        message: format!(
            "the transcription endpoint answered 200 with something that is not a \
             transcription — no `text` field. It sent: {excerpt}"
        ),
    })
}

/// A non-2xx, named.
///
/// Takes the status and the body and **nothing else** — in particular not the key, which is
/// what makes "a key never reaches a message" a property of the signature rather than of the
/// care taken at each call site.
fn hosted_error(provider: Provider, status: u16, body: &str) -> AgentError {
    let detail: String = body.trim().chars().take(300).collect();
    let detail = if detail.is_empty() { format!("HTTP {status}") } else { detail };
    if matches!(status, 401 | 403) {
        AgentError::Unauthorized { provider: provider.label().to_owned(), message: detail }
    } else {
        AgentError::Transport {
            transport: "http",
            message: format!("HTTP {status}: {detail}"),
        }
    }
}

// =======================================================================================
// The worker
// =======================================================================================

/// Transcribes on a worker thread, posting the answer back through a channel.
///
/// The shape every other pool in this codebase uses — `links.rs`, `transport/http.rs`,
/// `vellum-store`'s autosave — so the frame loop drains this alongside them and never blocks
/// on either a subprocess or an upload. Returns as soon as the thread is spawned.
///
/// The [`Utterance`] is moved in: it is plain data, which is exactly why the `!Send` capture
/// (see [`VoiceCapture`]) costs nothing.
pub fn transcribe_in_background(
    transcriber: Box<dyn Transcribe>,
    utterance: Utterance,
    events: Sender<VoiceEvent>,
) -> Result<JoinHandle<()>> {
    let node = utterance.node.clone();
    let _ = events.send(VoiceEvent::Transcribing {
        node: node.clone(),
        backend: transcriber.label(),
    });
    std::thread::Builder::new()
        .name("velm-voice".into())
        .spawn(move || {
            let event = match transcriber.transcribe(&utterance) {
                Ok(text) if text.trim().is_empty() => VoiceEvent::Failed {
                    node,
                    message: "nothing was said in that recording".to_owned(),
                },
                Ok(text) => {
                    VoiceEvent::Transcribed(Dictation { node, text: text.trim().to_owned() })
                }
                Err(error) => VoiceEvent::Failed { node, message: error.to_string() },
            };
            let _ = events.send(event);
        })
        .map_err(AgentError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voiced() -> AgentModel {
        AgentModel { voice: true, ..AgentModel::worker() }
    }

    fn target() -> VoiceTarget {
        VoiceTarget::new("42@7", &voiced()).expect("a voiced node has a target")
    }

    /// The error out of a refused [`Speech::transcriber`].
    ///
    /// `unwrap_err` needs `T: Debug` and there is deliberately none: [`HostedTranscriber`]
    /// holds a key and therefore has no `Debug` at all. So this is the `let … else` shape
    /// `transport/http.rs`'s own tests use, for exactly the same reason.
    fn refusal(result: Result<Box<dyn Transcribe>>) -> AgentError {
        match result {
            Ok(_) => panic!("a transcriber was built where one should have been refused"),
            Err(error) => error,
        }
    }

    // -----------------------------------------------------------------------------------
    // The policy
    // -----------------------------------------------------------------------------------

    /// The claim in the module's own doc comment, as an assertion. Constructing everything —
    /// the capture, the push-to-talk, the target — must open **no device**, and a release
    /// without a press must refuse rather than hand back a recording nobody asked for.
    #[test]
    fn nothing_is_captured_without_an_explicit_press() {
        let mut talk = PushToTalk::new(CannedCapture::tone());
        assert_eq!(talk.capture().starts, 0, "constructing a push-to-talk opened a device");
        assert!(!talk.capture().is_recording());
        assert_eq!(talk.holding(), None);

        let error = talk.release(0).expect_err("a release with no press handed back audio");
        assert!(error.to_string().contains("nothing was being recorded"), "{error}");
        assert_eq!(talk.capture().starts, 0, "a failed release opened a device");

        // And the press is what — the only thing that — turns it on.
        talk.press(&target(), 0).unwrap();
        assert_eq!(talk.capture().starts, 1);
        assert!(talk.capture().is_recording());
        assert_eq!(talk.holding(), Some("42@7"));

        // Cancelling closes it and keeps nothing.
        talk.cancel();
        assert!(!talk.capture().is_recording(), "cancel left the microphone open");
        assert_eq!(talk.holding(), None);
    }

    /// `AgentModel::voice` is off by default and it is the gate. A node that has not opted in
    /// has no way to be pressed, because there is no way to build the value `press` takes.
    #[test]
    fn a_node_that_did_not_opt_in_cannot_be_recorded_against() {
        let silent = AgentModel::worker();
        assert!(!silent.voice, "voice must default to off");
        assert!(VoiceTarget::new("42@7", &silent).is_none(), "a voiceless node gave a target");

        // And a node with no id cannot either — a dictation with nowhere to land is worse
        // than a refused press.
        assert!(VoiceTarget::new("", &voiced()).is_none());
        assert_eq!(VoiceTarget::new("42@7", &voiced()).unwrap().node(), "42@7");
    }

    /// A second press while one is live is refused rather than switching nodes: the first
    /// half of the sentence was addressed to somebody else.
    #[test]
    fn a_press_cannot_be_stolen_by_another_node() {
        let mut talk = PushToTalk::new(CannedCapture::tone());
        talk.press(&target(), 0).unwrap();

        let other = VoiceTarget::new("9@1", &voiced()).unwrap();
        let error = talk.press(&other, 100).expect_err("a second press was accepted");
        assert!(error.to_string().contains("42@7"), "the refusal did not name who has it: {error}");
        assert_eq!(talk.holding(), Some("42@7"));
        assert_eq!(talk.capture().starts, 1, "a refused press still opened a device");
    }

    // -----------------------------------------------------------------------------------
    // The whole path, with no microphone
    // -----------------------------------------------------------------------------------

    /// Press → release → transcribe → [`Dictation`], through the real types, with a fake
    /// capture and a fake transcriber. This is the half that has to be right, and it needs
    /// neither a device nor a network.
    #[test]
    fn a_fake_capture_reaches_text_end_to_end() {
        let mut talk = PushToTalk::new(CannedCapture::tone());
        talk.press(&target(), 1_000).unwrap();
        assert_eq!(talk.held_ms(1_400), 400);

        let utterance = talk.release(1_500).expect("a released press produced no audio");
        assert_eq!(utterance.node(), "42@7", "the utterance lost the node it was said to");
        assert_eq!(utterance.sample_rate(), TARGET_SAMPLE_RATE);
        assert_eq!(utterance.duration_ms(), 1_000, "a second of tone did not survive");
        assert!(!utterance.is_silent(), "a 440 Hz tone was read as silence");
        assert_eq!(talk.holding(), None, "the press was still held after a release");
        assert!(!talk.capture().is_recording(), "the release left the microphone open");

        let transcriber = CannedTranscriber::new("  open the door  ");
        let text = transcriber.transcribe(&utterance).unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        transcribe_in_background(Box::new(transcriber), utterance, sender)
            .unwrap()
            .join()
            .unwrap();

        let events: Vec<VoiceEvent> = receiver.iter().collect();
        assert!(
            matches!(&events[0], VoiceEvent::Transcribing { node, .. } if node == "42@7"),
            "{events:?}"
        );
        assert_eq!(
            events[1],
            VoiceEvent::Transcribed(Dictation {
                node: "42@7".into(),
                // Trimmed on the way out: this becomes a prompt, and a prompt with leading
                // whitespace is one an agent quotes back at you.
                text: "open the door".into(),
            }),
            "raw was {text:?}"
        );
    }

    /// Escape mid-press, then talk again. Nothing else drives the re-arm path, and it is the
    /// one the eraser's own abandoned-sweep bug (feedback 27) says to check: a gesture that
    /// ends without a release must leave the thing it held in a state the next gesture can
    /// use. A `cancel` that failed to close the device would make the second press refuse.
    #[test]
    fn a_cancelled_press_leaves_the_node_able_to_talk_again() {
        let mut talk = PushToTalk::new(CannedCapture::tone());
        talk.press(&target(), 0).unwrap();
        talk.cancel();
        assert_eq!(talk.capture().starts, 1);

        talk.press(&target(), 5_000).expect("a press after a cancel was refused");
        assert_eq!(talk.capture().starts, 2, "the second press did not open a device");
        let utterance = talk.release(6_000).expect("the second press produced nothing");
        assert_eq!(utterance.duration_ms(), 1_000, "the re-armed press recorded a short one");
        assert_eq!(utterance.node(), "42@7");
    }

    /// An empty recording is a refusal that names the likeliest cause, not an empty utterance
    /// that a transcriber then reports as *"the model heard nothing"* — which points at the
    /// wrong thing entirely.
    #[test]
    fn a_microphone_that_gave_nothing_says_so_rather_than_transcribing_silence() {
        let mut talk = PushToTalk::new(CannedCapture::new(Vec::new(), 48_000, 1));
        talk.press(&target(), 0).unwrap();
        let error = talk.release(500).expect_err("an empty recording became an utterance");
        let message = error.to_string();
        assert!(message.contains("recorded nothing"), "{message}");
        assert!(message.contains("Microphone"), "the remedy was not named: {message}");
        assert_eq!(talk.holding(), None, "a failed release left the press held");
    }

    /// A build without the audio backend refuses **by name**, with the remedy in it — the
    /// dead-button case `docs/07` §0.3 exists to prevent.
    #[test]
    fn a_build_without_the_feature_says_what_is_missing() {
        let mut fallback = Unavailable;
        let error = fallback.start().expect_err("the fallback capture recorded something");
        let message = error.to_string();
        assert!(message.contains("`voice` feature"), "the flag was not named: {message}");
        assert!(message.contains("not built into"), "the state was not named: {message}");
        // It must **not** quote a command line — see the constant's own note.
        assert!(!message.contains("cargo "), "a build command was promised: {message}");
        assert!(!fallback.is_recording());
        assert!(fallback.stop().is_err());

        // Cancelling a capture that never started is a no-op, not a panic: `cancel` is what
        // Escape and a tab switch call, and they arrive whatever state the node is in.
        let mut talk = PushToTalk::new(Unavailable);
        talk.cancel();
        assert_eq!(talk.holding(), None);
        assert!(talk.poll(u64::MAX).is_none());

        // The one place the two are chosen between agrees with the flag.
        assert_eq!(capture_is_built_in(), cfg!(feature = "voice"));
    }

    // -----------------------------------------------------------------------------------
    // The bound — two guards, one test each
    // -----------------------------------------------------------------------------------

    /// The buffer-side guard. A capture that hands back more than the bound is truncated by
    /// [`Recording::new`], so no implementation of the trait — present or future, real or
    /// fake — can produce an unbounded recording.
    #[test]
    fn a_capture_that_never_stops_is_cut_off_at_the_bound() {
        let rate = TARGET_SAMPLE_RATE;
        let bound = max_samples(rate, 1);
        assert_eq!(bound, rate as usize * MAX_UTTERANCE_SECONDS as usize);

        // Ten seconds past the cap.
        let overrun = vec![1_000i16; bound + rate as usize * 10];
        let recording = Recording::new(overrun, rate, 1);
        assert_eq!(recording.samples().len(), bound, "an over-long recording was kept whole");
        assert_eq!(recording.duration_ms(), MAX_UTTERANCE_SECONDS * 1000);

        // Stereo doubles the sample count for the same wall time, which is what makes the
        // bound a function of the format rather than a constant.
        assert_eq!(max_samples(48_000, 2), 48_000 * 2 * MAX_UTTERANCE_SECONDS as usize);
    }

    /// The clock-side guard, which is a different failure: a key that never comes up. `poll`
    /// answers `None` on every ordinary frame and releases exactly once when the press has
    /// outlived the bound.
    #[test]
    fn a_stuck_press_is_released_by_the_clock() {
        let mut talk = PushToTalk::new(CannedCapture::tone());
        talk.press(&target(), 10_000).unwrap();

        assert!(talk.poll(10_016).is_none(), "a fresh press was cut off");
        assert!(
            talk.poll(10_000 + MAX_UTTERANCE_SECONDS * 1000 - 1).is_none(),
            "a press was cut off a millisecond early"
        );
        assert_eq!(talk.holding(), Some("42@7"));

        let utterance = talk
            .poll(10_000 + MAX_UTTERANCE_SECONDS * 1000)
            .expect("a stuck press was never released")
            .expect("the forced release produced nothing");
        assert_eq!(utterance.node(), "42@7");
        assert_eq!(talk.holding(), None, "the press survived its own release");
        assert!(!talk.capture().is_recording(), "the microphone was left open");
        assert!(talk.poll(u64::MAX).is_none(), "poll released a second time");

        // A clock that goes backwards must not release: `saturating_sub` is what makes that
        // a non-event rather than an enormous elapsed time.
        let mut talk = PushToTalk::new(CannedCapture::tone());
        talk.press(&target(), 10_000).unwrap();
        assert!(talk.poll(9_000).is_none());
    }

    // -----------------------------------------------------------------------------------
    // The audio arithmetic
    // -----------------------------------------------------------------------------------

    /// The full 44-byte canonical header, field by field. The two lengths are the fields that
    /// go wrong, and a player opens the file anyway when they are — so this is the only place
    /// the mistake is visible.
    #[test]
    fn a_wav_is_the_44_canonical_header_bytes_then_little_endian_samples() {
        let wav = wav_bytes(&[0, 1, -1, 32_767], TARGET_SAMPLE_RATE);
        assert_eq!(wav.len(), 44 + 8);

        assert_eq!(&wav[0..4], b"RIFF");
        // 36 + data, i.e. everything after these first eight bytes — **not** the file length.
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16, "fmt chunk length");
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1, "1 = uncompressed");
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1, "channels");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000, "sample rate");
        // 16000 × 1 × 16 / 8.
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000, "byte rate");
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2, "block align");
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16, "bits per sample");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 8, "data length");

        assert_eq!(&wav[44..52], &[0, 0, 1, 0, 0xFF, 0xFF, 0xFF, 0x7F]);

        // An empty recording is still a valid file rather than a truncated one.
        let empty = wav_bytes(&[], TARGET_SAMPLE_RATE);
        assert_eq!(empty.len(), 44);
        assert_eq!(u32::from_le_bytes(empty[4..8].try_into().unwrap()), 36);
    }

    /// Down and up, and the boundary case that indexes off the end.
    #[test]
    fn resampling_lands_on_the_rate_the_transcribers_want() {
        // 48 kHz → 16 kHz: every third sample, six in, two out.
        let down = resample(&[0, 10, 20, 30, 40, 50], 48_000, 16_000);
        assert_eq!(down, vec![0, 30]);

        // 8 kHz → 16 kHz: one interpolated sample between each pair, and the last value is
        // **held** rather than wrapping round to the start of the recording.
        let up = resample(&[0, 100], 8_000, 16_000);
        assert_eq!(up, vec![0, 50, 100, 100]);

        assert_eq!(resample(&[1, 2, 3], 16_000, 16_000), vec![1, 2, 3], "a no-op resample copied");
        assert!(resample(&[], 48_000, 16_000).is_empty());
        assert!(resample(&[1, 2], 0, 16_000).is_empty(), "a zero rate must not divide by it");

        // A real length: one second at 44.1 kHz is one second at 16 kHz.
        let second = vec![0i16; 44_100];
        assert_eq!(resample(&second, 44_100, TARGET_SAMPLE_RATE).len(), 16_000);
    }

    /// Averaged, not "take channel one" — a machine with the microphone on input 2 would
    /// otherwise record silence and look like a broken feature.
    #[test]
    fn a_stereo_recording_is_averaged_down_rather_than_half_discarded() {
        // Left silent, right carrying everything.
        let stereo = Recording::new(vec![0, 100, 0, 200, 0, 300], 16_000, 2);
        assert_eq!(stereo.duration_ms(), 0, "three frames is under a millisecond");
        let utterance = stereo.to_utterance("n");
        assert_eq!(utterance.samples(), &[50, 100, 150]);

        // Mono is passed through untouched.
        let mono = Recording::new(vec![7, 8, 9], TARGET_SAMPLE_RATE, 1);
        assert_eq!(mono.to_utterance("n").samples(), &[7, 8, 9]);
    }

    #[test]
    fn silence_is_reported_and_speech_is_not() {
        assert!(Utterance::new("n", vec![0; 100], TARGET_SAMPLE_RATE).is_silent());
        assert!(Utterance::new("n", vec![5, -7, 3], TARGET_SAMPLE_RATE).is_silent());
        assert!(!Utterance::new("n", vec![0, 4_000], TARGET_SAMPLE_RATE).is_silent());
        // `i16::MIN.abs()` overflows; `saturating_abs` is what keeps the loudest possible
        // sample from panicking a release build into an abort.
        assert!(!Utterance::new("n", vec![i16::MIN], TARGET_SAMPLE_RATE).is_silent());
    }

    // -----------------------------------------------------------------------------------
    // Choosing a backend
    // -----------------------------------------------------------------------------------

    /// The privacy default, as an assertion. If this ever inverts, every user's voice starts
    /// leaving their machine on a setting they never changed.
    #[test]
    fn local_is_preferred_whenever_it_is_installed() {
        assert_eq!(Preference::default(), Preference::Auto);

        assert_eq!(choose_backend(Preference::Auto, true, true).unwrap(), Backend::Local);
        assert_eq!(choose_backend(Preference::Auto, true, false).unwrap(), Backend::Local);
        assert_eq!(choose_backend(Preference::Auto, false, true).unwrap(), Backend::Hosted);

        // And an explicit choice is honoured in both directions, including the one that
        // refuses rather than quietly uploading.
        assert_eq!(choose_backend(Preference::Hosted, true, true).unwrap(), Backend::Hosted);
        assert_eq!(choose_backend(Preference::Local, true, true).unwrap(), Backend::Local);
        let refused = choose_backend(Preference::Local, false, true)
            .expect_err("a local-only setting uploaded the recording");
        assert!(refused.to_string().contains("whisper"), "{refused}");
    }

    /// With nothing set up, the message has to name **both** ways out and say plainly that
    /// nothing was sent anywhere — that last clause is the whole difference between a refusal
    /// a user trusts and one they wonder about.
    #[test]
    fn with_no_backend_configured_the_message_is_actionable() {
        let error = choose_backend(Preference::Auto, false, false)
            .expect_err("a transcriber appeared from nowhere");
        let message = error.to_string();
        assert!(message.contains("whisper"), "the local remedy was not named: {message}");
        assert!(
            message.contains("library.json"),
            "the hosted remedy must name a place that exists: {message}"
        );
        assert!(message.contains("Nothing was recorded to anywhere"), "{message}");

        let hosted_only = choose_backend(Preference::Hosted, true, false).unwrap_err();
        assert!(hosted_only.to_string().contains("none is configured"), "{hosted_only}");
    }

    /// A `Speech` that has been set up round-trips, and an unconfigured one writes nothing —
    /// the file-format posture `model.rs` states for every token in this crate.
    #[test]
    fn the_setting_round_trips_and_a_default_writes_nothing() {
        assert_eq!(serde_json::to_string(&Speech::default()).unwrap(), "{}");

        let configured = Speech {
            preference: Preference::Hosted,
            command: Some("whisper-cli".into()),
            model_file: Some("/models/ggml-base.en.bin".into()),
            args: vec!["-m".into(), "{model}".into(), "-f".into(), "{audio}".into()],
            hosted_provider: Some(Provider::OpenAi),
            hosted_base_url: Some("http://localhost:8080/v1".into()),
            hosted_model: Some("a-transcription-model".into()),
        };
        let json = serde_json::to_string(&configured).unwrap();
        assert_eq!(serde_json::from_str::<Speech>(&json).unwrap(), configured);
        assert!(!json.contains("key"), "the setting grew somewhere to put a secret: {json}");

        for preference in Preference::ALL {
            let json = serde_json::to_string(&preference).unwrap();
            assert_eq!(serde_json::from_str::<Preference>(&json).unwrap(), preference);
        }
    }

    /// The two providers with no speech endpoint are refused **by name**, with the two ways
    /// out in the sentence — pointing them at the OpenAI path would 404 and read as a bad key.
    #[test]
    fn a_provider_with_no_speech_endpoint_is_refused_by_name() {
        for provider in [Provider::Claude, Provider::Gemini] {
            let speech = Speech {
                preference: Preference::Hosted,
                hosted_provider: Some(provider),
                hosted_model: Some("a-model".into()),
                ..Speech::default()
            };
            let message = refusal(speech.transcriber(None, false)).to_string();
            assert!(message.contains(provider.label()), "{message}");
            assert!(message.contains("whisper"), "the local way out was not offered: {message}");
        }
    }

    /// A hosted endpoint with no model named refuses and explains, rather than guessing one.
    #[test]
    fn an_unnamed_transcription_model_is_refused_rather_than_guessed() {
        let speech = Speech {
            preference: Preference::Hosted,
            hosted_base_url: Some("http://localhost:8080/v1".into()),
            ..Speech::default()
        };
        let error = refusal(speech.transcriber(None, false));
        assert!(error.to_string().contains("name the transcription model"), "{error}");
    }

    // -----------------------------------------------------------------------------------
    // The key
    // -----------------------------------------------------------------------------------

    /// The one thing in this module that must never be printed. Every string it can produce
    /// is checked, including the **upload body** — which is where a key would go if anyone
    /// ever "helpfully" added it as a form field.
    #[test]
    fn an_api_key_never_reaches_anything_this_module_produces() {
        const KEY: &str = "sk-a-secret-that-must-not-appear";

        let scratch = tempfile::tempdir().unwrap();
        Credentials::store(scratch.path(), Provider::OpenAi, KEY).unwrap();

        let speech = Speech {
            preference: Preference::Hosted,
            hosted_provider: Some(Provider::OpenAi),
            hosted_model: Some("a-transcription-model".into()),
            ..Speech::default()
        };
        let transcriber = speech
            .transcriber(Some(scratch.path()), false)
            .expect("a configured hosted backend was refused");

        assert!(!transcriber.label().contains(KEY), "the label leaked it");
        assert_eq!(transcriber.backend(), Backend::Hosted);

        // The setting itself has nowhere to put one.
        let printed = format!("{speech:?}");
        assert!(!printed.contains(KEY), "the setting printed it: {printed}");
        assert!(!serde_json::to_string(&speech).unwrap().contains(KEY));

        // The body that actually goes on the wire.
        let wav = Utterance::new("n", vec![1, 2, 3], TARGET_SAMPLE_RATE).wav();
        let body = multipart_body("a-boundary", "a-transcription-model", &wav);
        assert!(!contains(&body, KEY.as_bytes()), "the key was put in the upload body");

        // And every error the hosted path can raise.
        for status in [400u16, 401, 403, 500] {
            let error = hosted_error(Provider::OpenAi, status, "the provider's own words");
            assert!(!error.to_string().contains(KEY), "{error}");
            assert!(error.to_string().contains("provider's own words"), "{error}");
        }
        assert!(matches!(
            hosted_error(Provider::OpenAi, 401, "nope"),
            AgentError::Unauthorized { .. }
        ));
        assert!(matches!(
            hosted_error(Provider::OpenAi, 500, "nope"),
            AgentError::Transport { .. }
        ));

        // A hosted backend with no key at all refuses before it can send anything.
        let empty = tempfile::tempdir().unwrap();
        if std::env::var("OPENAI_API_KEY").is_err() {
            let error = refusal(speech.transcriber(Some(empty.path()), false));
            assert!(matches!(error, AgentError::Unauthorized { .. }), "{error}");
            assert!(!error.to_string().contains(KEY), "{error}");
        }
    }

    /// `transport/http.rs`'s rule, applied here: nothing in this file writes a model id, so
    /// there is nothing to go stale — and the refusal above is only enforceable while that
    /// stays true.
    #[test]
    fn no_model_id_is_written_down_in_this_module() {
        let source = include_str!("voice.rs");
        // Joined at runtime from halves, or the assertion finds its own haystack.
        for shape in [
            ["whisper", "-1"],
            ["gpt", "-4o-transcribe"],
            ["ggml", "-large"],
            ["claude", "-"],
            ["moonshot", "-v"],
        ] {
            let needle = shape.concat();
            assert!(!source.contains(&needle), "a model id reached the source: {needle}");
        }
    }

    // -----------------------------------------------------------------------------------
    // The two backends' own arithmetic
    // -----------------------------------------------------------------------------------

    /// The exact command line, which is otherwise only observable by watching a process list.
    #[test]
    fn the_local_command_line_is_rendered_from_the_template() {
        let template: Vec<String> =
            DEFAULT_LOCAL_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
        let audio = std::path::Path::new("/tmp/velm-voice-1-0.wav");

        let rendered = render_args(&template, Some("/models/base.bin"), audio).unwrap();
        assert_eq!(
            rendered,
            vec!["-m", "/models/base.bin", "-f", "/tmp/velm-voice-1-0.wav", "-nt", "-np"]
        );

        // A template that forgot the audio gets it appended — a transcriber with no file
        // argument is a call that cannot work.
        let forgetful = vec!["--stdout".to_owned()];
        assert_eq!(
            render_args(&forgetful, None, audio).unwrap(),
            vec!["--stdout", "/tmp/velm-voice-1-0.wav"]
        );

        // A `{model}` with nothing to put in it refuses, rather than handing the tool a `-m`
        // with no value and an error the user cannot act on.
        let error = render_args(&template, None, audio).expect_err("an empty model was accepted");
        assert!(error.to_string().contains("model file"), "{error}");
        assert!(render_args(&template, Some("   "), audio).is_err(), "whitespace passed as a model");
    }

    /// The three shapes real whisper builds produce. The third is the one that matters: a
    /// recording of silence comes back as `[BLANK_AUDIO]`, and that tag becoming an agent's
    /// prompt is a turn nobody asked for.
    #[test]
    fn a_transcript_is_cleaned_of_timestamps_and_tags() {
        assert_eq!(clean_transcript("  Open the door.  \n"), "Open the door.");

        let timestamped = "[00:00:00.000 --> 00:00:02.000]   Open the door.\n\
                           [00:00:02.000 --> 00:00:04.000]   Then close it.\n";
        assert_eq!(clean_transcript(timestamped), "Open the door. Then close it.");

        assert_eq!(clean_transcript("[BLANK_AUDIO]\n"), "");
        assert_eq!(clean_transcript("[00:00:00.000 --> 00:00:02.000]   [BLANK_AUDIO]\n"), "");
        assert_eq!(clean_transcript("\n\n   \n"), "");
        // Whitespace inside a line is collapsed: whisper pads its segments.
        assert_eq!(clean_transcript("a    b\n   c  "), "a b c");
    }

    /// The multipart body, byte for byte where it matters. CRLF everywhere, a blank line
    /// before each part's content, and **two** trailing hyphens on the closing delimiter —
    /// which is the one a server rejects with a message about a malformed request.
    #[test]
    fn the_multipart_body_is_well_formed_and_carries_the_audio_intact() {
        let wav = wav_bytes(&[1, 2, 3], TARGET_SAMPLE_RATE);
        let body = multipart_body("B0UND", "a-transcription-model", &wav);
        let text = String::from_utf8_lossy(&body);

        assert!(text.starts_with("--B0UND\r\n"), "{}", &text[..40.min(text.len())]);
        assert!(text.contains("Content-Disposition: form-data; name=\"model\"\r\n\r\na-transcription-model\r\n"));
        assert!(text.contains("name=\"response_format\"\r\n\r\njson\r\n"));
        assert!(text.contains(
            "Content-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        ));
        assert!(text.ends_with("\r\n--B0UND--\r\n"), "the closing delimiter is wrong");
        assert!(contains(&body, &wav), "the audio did not survive the encoding");

        // The boundary must not occur in the payload, or the upload truncates there and the
        // transcript is of the first half of the sentence.
        let boundary = unique_boundary(&wav);
        assert!(!contains(&wav, boundary.as_bytes()));
    }

    /// ⚠ **The version of this that lived inside the multipart test was vacuous**, and it is
    /// worth saying how: it built a hostile payload out of the boundary's *prefix*
    /// (`velmvoice{pid}`) and asserted the answer did not occur in it. The generated name is
    /// longer than that prefix, so `contains` returned `false` on its length check before
    /// comparing a single byte — the retry loop it was written to exercise never ran once.
    ///
    /// The loop is the thing that matters: PCM is arbitrary bytes, so a boundary that *does*
    /// occur in the audio truncates the upload at that point and produces a transcript of the
    /// first half of the sentence — a bug nobody would ever diagnose from the symptom.
    ///
    /// A/B: with the loop's body removed, the first assertion fails.
    #[test]
    fn a_boundary_that_occurs_in_the_payload_is_moved_until_it_does_not() {
        // A payload that contains the whole starting name, which is what the loop is for.
        let hostile = b"....velmvoice1....".to_vec();
        let dodged = dodge_boundary("velmvoice1".to_owned(), &hostile);
        assert_ne!(dodged, "velmvoice1", "the collision was not dodged at all");
        assert!(!contains(&hostile, dodged.as_bytes()), "the boundary occurs in the payload");

        // And when each successive attempt is also present, so the loop has to go round more
        // than once — one retry is not evidence that a second one works.
        let mut stubborn = b"velmvoice1".to_vec();
        stubborn.extend_from_slice(b" velmvoice1x0 velmvoice1x0x1 velmvoice1x0x1x2");
        let dodged = dodge_boundary("velmvoice1".to_owned(), &stubborn);
        assert!(!contains(&stubborn, dodged.as_bytes()), "{dodged}");
        assert!(dodged.len() > "velmvoice1x0x1x2".len(), "the loop stopped early: {dodged}");

        // A payload with no collision leaves the name exactly as it was.
        assert_eq!(dodge_boundary("velmvoice1".to_owned(), b"nothing here"), "velmvoice1");
    }

    /// ⚠ **This test asserted the bug, in both of its last two lines.** It required the raw
    /// body to become the transcript (*"Open the door.\n"*) and required a JSON error body to
    /// come back as a *successful* transcript containing the words *"no such"*.
    ///
    /// What that produced in the running app: a 200 that is not a transcription — a captive
    /// portal's login page, a proxy's error document, a server's own index — became, verbatim
    /// and uncapped, **what the user said**, and went straight into the agent as its next
    /// prompt. There is nothing to disbelieve at that point; the words are in the box.
    #[test]
    fn a_transcription_that_is_not_a_transcription_is_a_named_failure() {
        assert_eq!(read_transcription(r#"{"text":"  Open the door. "}"#).unwrap(), "Open the door.");
        // An empty transcription is a legitimate answer — the user said nothing — and must
        // not be confused with a body that had no `text` field at all.
        assert_eq!(read_transcription(r#"{"text":""}"#).unwrap(), "");

        // The captive portal, which is the case this exists for. It must be an **error** —
        // the excerpt is deliberately quoted, because naming what came back instead is the
        // whole value of the message; what must never happen is the page arriving as a
        // successful transcript and going into the agent as the user's words.
        let portal = "<!doctype html><title>Sign in to WiFi</title><h1>Sign in</h1>";
        let error = read_transcription(portal).unwrap_err();
        assert!(error.to_string().contains("not a transcription"), "{error}");

        let plain = read_transcription("Open the door.\n").unwrap_err();
        assert!(plain.to_string().contains("not a transcription"), "{plain}");

        // A provider that named the problem has its words quoted, because that is the one
        // thing worth putting in front of the user.
        let named = read_transcription(r#"{"error":{"message":"no such model"}}"#).unwrap_err();
        assert!(named.to_string().contains("no such model"), "{named}");
        let bare = read_transcription(r#"{"error":"no such model"}"#).unwrap_err();
        assert!(bare.to_string().contains("no such model"), "{bare}");

        // Capped by **characters**. A megabyte of HTML must not reach a toast, and a byte
        // slice through a multi-byte character aborts the process outright.
        let huge = format!("<html>{}</html>", "\u{4e2d}".repeat(5_000));
        let capped = read_transcription(&huge).unwrap_err().to_string();
        assert!(capped.chars().count() < 400, "{} characters", capped.chars().count());
    }

    /// The temp WAV is private and **removes itself**, including on the paths that return
    /// early — a recording of somebody talking must not be what gets left in `/tmp`.
    #[test]
    fn the_temporary_wav_is_private_and_cleans_up_after_itself() {
        let path = {
            let temp = TempWav::write(&wav_bytes(&[1, 2, 3], TARGET_SAMPLE_RATE)).unwrap();
            assert!(temp.path.exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&temp.path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "a recording was written world-readable: {mode:o}");
            }
            temp.path.clone()
        };
        assert!(!path.exists(), "the recording was left behind at {}", path.display());

        // Two of them never collide, which a name built from the clock at one-second
        // granularity would not guarantee.
        let first = TempWav::write(b"a").unwrap();
        let second = TempWav::write(b"b").unwrap();
        assert_ne!(first.path, second.path);
    }

    /// A backend that is not installed is reported as a missing **command**, which is the
    /// error variant whose whole point is naming what to install.
    #[test]
    fn a_missing_local_binary_names_itself() {
        let speech = Speech {
            preference: Preference::Local,
            command: Some("velm-definitely-not-a-transcriber".into()),
            ..Speech::default()
        };
        assert!(speech.detect_local().is_none());
        // `choose_backend` refuses first, because the preference says local only.
        let error = refusal(speech.transcriber(None, false));
        assert!(error.to_string().contains("whisper"), "{error}");

        // With availability asserted but the binary really absent, the refusal names it.
        let error = refusal(speech.transcriber(None, true));
        assert!(
            error.to_string().contains("velm-definitely-not-a-transcriber"),
            "the missing binary was not named: {error}"
        );
        assert!(matches!(error, AgentError::MissingCommand { .. }));
    }

    /// ⚠ **Feature 18's last step, run on a real file.** `transcribe_media` was wired into both
    /// ingest paths and had never been executed against anything — the shape this repository
    /// keeps finding, where every part is tested and the join is not.
    ///
    /// The "transcriber" is a shell script that echoes a line, which is exactly what
    /// `LocalTranscriber` expects of one: a command that takes the rendered arguments and
    /// prints text. So this drives the real `Speech::detect_local`, the real `render_args`, the
    /// real process spawn and the real `clean_transcript`, with no whisper installed and no
    /// network — and it asserts the **media file's own path** reached the command, which is the
    /// one thing that distinguishes this from transcribing a recording.
    #[test]
    #[cfg(unix)]
    fn a_media_file_is_transcribed_by_the_tool_on_this_machine() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir().unwrap();
        let media = scratch.path().join("interview.m4a");
        std::fs::write(&media, b"not really audio, and the script does not care").unwrap();

        // Echoes the words, and the path it was handed, so the assertion can prove the file
        // reached the tool rather than a temporary WAV built from nothing.
        let tool = scratch.path().join("fake-whisper");
        let mut script = std::fs::File::create(&tool).unwrap();
        writeln!(script, "#!/bin/sh\necho \"heard: $*\"").unwrap();
        drop(script);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let speech = Speech {
            command: Some(tool.display().to_string()),
            model_file: Some("ggml-tiny.bin".into()),
            ..Speech::default()
        };

        let text = transcribe_media(&speech, &media).expect("the script transcribes");
        assert!(text.contains("heard:"), "the tool's output did not come back: {text:?}");
        assert!(
            text.contains("interview.m4a"),
            "the media file's own path never reached the tool: {text:?}"
        );

        // And with nothing installed it refuses **by name**, rather than silently attaching
        // the file with no words — which is the state the node's message describes.
        let bare = Speech { command: Some("velm-no-such-transcriber".into()), ..Speech::default() };
        let refused = transcribe_media(&bare, &media).expect_err("nothing is installed");
        assert!(
            refused.to_string().contains("this machine"),
            "the refusal must name the remedy: {refused}"
        );
    }
}
