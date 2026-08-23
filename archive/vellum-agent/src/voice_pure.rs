//! The half of [`crate::voice`] that is only vocabulary.
//!
//! `vellum-ui` draws the Voice rows in Preferences and the "not built in" note on an agent
//! node, and the chrome has to build on **every** target — including `wasm32`, where a
//! browser tab has no microphone device, no `whisper-cli` to probe for and no `ureq`. The
//! capture and transcription code in [`crate::voice`] cannot exist there; these two items
//! can, and they are all the chrome ever needed.
//!
//! This is `voice::Unavailable`'s own precedent applied one level up (not a link: under
//! `not(feature = "native")` the `voice` module is the alias below and has no such item): **the
//! capability is gated, the vocabulary is not.** The public path is unchanged either way —
//! `vellum_agent::voice::Preference` resolves here on both targets — so no caller can tell
//! which half it got, and nothing goes quiet: a build without the capability still has the
//! words to say so by name.

use serde::{Deserialize, Serialize};

/// What the fallback capture says when the feature was not built in.
///
/// Actionable rather than merely true: it names the feature, because the person most likely
/// to read it is building Velm themselves.
///
/// ⚠ It names the **feature**, not a command line, and that is deliberate. The exact
/// invocation depends on how `vellum-app` passes the flag through and on how `build.py`
/// invokes cargo — neither of which this crate can see — so a command quoted here would be a
/// remedy that is wrong the first time somebody renames the passthrough. Feedback 18 is the
/// worked example of a named-but-wrong remedy costing more than no remedy at all.
pub const NOT_BUILT_IN: &str = "voice capture is not built into this copy of Velm — it needs \
     a build with the `voice` feature turned on; everything else about this node works \
     without it";

/// Which transcriber to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Preference {
    /// Local when it is installed, hosted otherwise. **The default**, and the local half of
    /// it is not a tie-break — see this module's note on why.
    #[default]
    Auto,
    /// Local only. A refusal when the binary is missing, rather than a quiet upload.
    Local,
    /// Hosted only. For a user who has no local model and has said so.
    Hosted,
}

impl Preference {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Local, Self::Hosted];

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Prefer on this machine",
            Self::Local => "Only on this machine",
            Self::Hosted => "Only the API",
        }
    }
}

