//! Decoder for Miro's clipboard payload.
//!
//! Copying objects in Miro puts an HTML flavour on the system clipboard. Buried in
//! it is a `data-meta` attribute holding Miro's *internal* widget model — the same
//! shape as the `canvas.json` inside a `.rtb` backup, which ships encrypted and is
//! therefore unreadable (see `docs/02-miro-formats.md`).
//!
//! The clipboard copy is not encrypted, only obfuscated:
//!
//! ```text
//! data-meta="<--(miro-data-v1)<base64>"
//!                             └─ base64 → add a constant to every byte → UTF-8 JSON
//! ```
//!
//! Observed constant is 197 (mod 256). We do **not** hardcode it: [`decode`] recovers
//! it by trying all 256 values and keeping the one that yields parseable JSON. A
//! single byte-add is trivially brute-forced, so self-calibrating costs nothing and
//! survives Miro changing the constant. See [`Decoded::byte_shift`] to observe what
//! was actually used.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Marker introducing the payload. The trailing digit is a format version we
/// deliberately check, so a future `miro-data-v2` fails loudly instead of being
/// silently mis-parsed into a mangled board.
///
/// The payload is *delimited*, comment-style, not merely prefixed:
/// `<--(miro-data-v1)…base64…(/miro-data-v1)-->`. Small copies happen to be
/// truncated by the clipboard before the closing marker, which is why it is easy
/// to miss; a full-board copy carries it.
const MARKER_PREFIX: &str = "<--(miro-data-v";

/// Format versions this decoder is known to handle.
const SUPPORTED_MARKER_VERSIONS: &[&str] = &["1"];

/// A decoded clipboard payload, plus how it was decoded.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// Miro's format version from the marker, e.g. `"1"` for `miro-data-v1`.
    pub marker_version: String,
    /// The byte offset that turned the base64 blob into valid JSON. Recorded rather
    /// than assumed; if this ever differs from 197 we want it visible, not silent.
    pub byte_shift: u8,
    /// The payload itself.
    pub payload: serde_json::Value,
}

impl Decoded {
    /// Miro's own board id, e.g. `"bTBja0JvYXJkSWQ="`. Useful for grouping several
    /// pastes that came from the same source board.
    pub fn board_id(&self) -> Option<&str> {
        self.payload.get("boardId")?.as_str()
    }

    /// The copied objects, in clipboard order.
    pub fn objects(&self) -> &[serde_json::Value] {
        self.payload
            .get("data")
            .and_then(|d| d.get("objects"))
            .and_then(|o| o.as_array())
            .map_or(&[], |v| v.as_slice())
    }
}

/// Extracts and decodes a Miro payload from an HTML clipboard flavour.
///
/// Returns `Ok(None)` when the HTML simply isn't from Miro — the common case when
/// the user pastes from anywhere else, and not an error. Returns `Err` when the
/// payload *is* Miro's but we could not read it, which is a real failure worth
/// surfacing.
pub fn decode(clipboard_html: &str) -> Result<Option<Decoded>> {
    let Some(attr) = find_data_meta(clipboard_html) else {
        return Ok(None);
    };
    let attr = unescape_html_attr(&attr);

    let Some(rest) = attr.strip_prefix(MARKER_PREFIX) else {
        return Ok(None);
    };
    // `1)<base64…>` → version `1`, then the payload after the closing paren.
    let (version, b64) = rest
        .split_once(')')
        .ok_or_else(|| anyhow!("miro marker `{MARKER_PREFIX}` is not closed by `)`"))?;

    if !SUPPORTED_MARKER_VERSIONS.contains(&version) {
        bail!(
            "unsupported Miro clipboard format `miro-data-v{version}` \
             (this decoder handles {SUPPORTED_MARKER_VERSIONS:?}). \
             Miro likely changed the format; fall back to the SVG importer."
        );
    }

    // Trim the closing `(/miro-data-vN)` and any `-->` after it. Base64 decoding is
    // strict, so leaving these in corrupts the payload rather than being ignored.
    let closing = format!("(/miro-data-v{version})");
    let b64 = match b64.find(&closing) {
        Some(at) => &b64[..at],
        None => b64,
    };

    let bytes = decode_base64(b64.trim())?;
    let (payload, byte_shift) = deobfuscate(&bytes)?;

    Ok(Some(Decoded {
        marker_version: version.to_string(),
        byte_shift,
        payload,
    }))
}

/// Pulls the raw `data-meta="…"` attribute value out of the clipboard HTML.
///
/// Hand-rolled rather than pulling in an HTML parser: the attribute value is
/// base64 plus the marker, so it never contains a `"` to terminate early, and this
/// runs over multi-hundred-KB clipboard strings on the paste path.
fn find_data_meta(html: &str) -> Option<String> {
    let start = html.find("data-meta=\"")? + "data-meta=\"".len();
    let len = html[start..].find('"')?;
    Some(html[start..start + len].to_string())
}

/// Resolves the XML entities that survive on the clipboard. The payload is base64
/// (`A-Za-z0-9+/=`), so only `&amp;` can realistically appear inside it — but the
/// marker's `<--(` arrives as `&lt;--(`, which must be resolved for the prefix
/// match to succeed. `&amp;` is resolved last so `&amp;lt;` doesn't become `<`.
fn unescape_html_attr(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Decodes the payload, padding first since Miro emits it unpadded.
///
/// Deliberately strict. An earlier lenient version stripped stray non-alphabet
/// characters, which would have silently produced a corrupt board when the closing
/// `(/miro-data-v1)` marker was left in the input — instead the strict decoder
/// surfaced it as an error and the real bug got fixed. Garbage in this payload
/// means we have misunderstood the format, and that must be loud.
fn decode_base64(b64: &str) -> Result<Vec<u8>> {
    // A length of 4n+1 cannot be valid base64 no matter how it is padded, and
    // signals a truncated or mis-delimited payload.
    if b64.len() % 4 == 1 {
        bail!(
            "miro clipboard payload has an impossible base64 length ({} bytes); \
             it is truncated or the delimiters were parsed wrongly",
            b64.len()
        );
    }
    let mut padded = b64.to_string();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    STANDARD.decode(&padded).with_context(|| {
        let offending: String = b64.chars().filter(|c| !is_base64_char(*c)).take(8).collect();
        if offending.is_empty() {
            "miro clipboard payload is not valid base64".to_string()
        } else {
            format!("miro clipboard payload contains non-base64 characters: {offending:?}")
        }
    })
}

fn is_base64_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')
}

/// Recovers the JSON by finding the byte offset Miro applied.
///
/// Every candidate is checked by actually parsing the result, so a wrong offset
/// cannot produce a plausible-looking board. The observed offset is tried first so
/// the normal path costs one attempt.
fn deobfuscate(bytes: &[u8]) -> Result<(serde_json::Value, u8)> {
    const OBSERVED_SHIFT: u8 = 197;

    let candidates = std::iter::once(OBSERVED_SHIFT).chain((0u8..=255).filter(|s| *s != OBSERVED_SHIFT));

    for shift in candidates {
        let shifted: Vec<u8> = bytes.iter().map(|b| b.wrapping_add(shift)).collect();

        // Cheap reject before the full UTF-8 + JSON parse: real payloads are a
        // JSON object, so the first byte must be `{`.
        if shifted.first() != Some(&b'{') {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&shifted) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        // Guard against a degenerate parse (e.g. bare `{}`): insist on the fields
        // every real payload carries.
        if value.get("data").is_none() || value.get("boardId").is_none() {
            continue;
        }
        return Ok((value, shift));
    }

    bail!(
        "could not decode the Miro clipboard payload at any byte offset — \
         the obfuscation scheme has probably changed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a payload the way Miro does — including the closing delimiter, which
    /// a real full-board copy carries and which broke the first implementation.
    fn encode_like_miro(json: &str, shift: u8) -> String {
        let bytes: Vec<u8> = json.bytes().map(|b| b.wrapping_sub(shift)).collect();
        let b64 = STANDARD.encode(&bytes);
        format!("<span data-meta=\"&lt;--(miro-data-v1){b64}(/miro-data-v1)--&gt;\"></span>")
    }

    /// Regression: the payload is delimited comment-style, not merely prefixed.
    /// Feeding the closing marker into base64 corrupts a real 1MB board copy.
    #[test]
    fn strips_the_closing_delimiter() {
        let html = encode_like_miro(SAMPLE, 197);
        assert!(html.contains("(/miro-data-v1)"), "fixture must carry the closing marker");
        let got = decode(&html).unwrap().expect("should decode despite the closing marker");
        assert_eq!(got.board_id(), Some("bTBja0JvYXJkSWQ="));
    }

    /// Small copies can arrive without the closing marker, so both must work.
    #[test]
    fn decodes_without_a_closing_delimiter() {
        let bytes: Vec<u8> = SAMPLE.bytes().map(|b| b.wrapping_sub(197)).collect();
        let b64 = STANDARD.encode(&bytes);
        let html = format!("<span data-meta=\"&lt;--(miro-data-v1){b64}\"></span>");
        assert_eq!(decode(&html).unwrap().unwrap().board_id(), Some("bTBja0JvYXJkSWQ="));
    }

    /// Corruption must fail loudly rather than yield a plausible-but-wrong board.
    #[test]
    fn non_base64_characters_are_named_in_the_error() {
        let html = "<span data-meta=\"&lt;--(miro-data-v1)AAAA!!!!AAAA\"></span>";
        let err = decode(html).unwrap_err().to_string();
        assert!(err.contains("non-base64") || err.contains("not valid base64"), "got: {err}");
    }

    const SAMPLE: &str = r#"{"boardId":"bTBja0JvYXJkSWQ=","data":{"objects":[]},"version":2}"#;

    #[test]
    fn decodes_a_miro_payload() {
        let html = encode_like_miro(SAMPLE, 197);
        let got = decode(&html).unwrap().expect("should be recognised as Miro");
        assert_eq!(got.board_id(), Some("bTBja0JvYXJkSWQ="));
        assert_eq!(got.byte_shift, 197);
        assert_eq!(got.marker_version, "1");
    }

    /// The whole point of self-calibrating: a changed constant must still decode.
    #[test]
    fn recovers_when_miro_changes_the_shift() {
        for shift in [0u8, 1, 59, 128, 197, 255] {
            let html = encode_like_miro(SAMPLE, shift);
            let got = decode(&html).unwrap().expect("should decode");
            assert_eq!(got.byte_shift, shift, "failed to recover shift {shift}");
            assert_eq!(got.board_id(), Some("bTBja0JvYXJkSWQ="));
        }
    }

    #[test]
    fn non_miro_html_is_not_an_error() {
        assert!(decode("<p>hello</p>").unwrap().is_none());
        assert!(decode("<span data-meta=\"something-else\"></span>").unwrap().is_none());
    }

    /// A future format must fail loudly rather than silently import a wrong board.
    #[test]
    fn unknown_format_version_is_an_error() {
        let err = decode("<span data-meta=\"&lt;--(miro-data-v9)AAAA\"></span>").unwrap_err();
        assert!(err.to_string().contains("miro-data-v9"), "got: {err}");
    }

    #[test]
    fn corrupt_payload_is_an_error() {
        // Valid base64, but no byte offset turns it into a Miro payload.
        let b64 = STANDARD.encode([0u8; 64]);
        let html = format!("<span data-meta=\"&lt;--(miro-data-v1){b64}\"></span>");
        assert!(decode(&html).is_err());
    }

    #[test]
    fn objects_are_exposed_in_order() {
        let json = r#"{"boardId":"b","data":{"objects":[{"type":1},{"type":2}]}}"#;
        let html = encode_like_miro(json, 197);
        let got = decode(&html).unwrap().unwrap();
        assert_eq!(got.objects().len(), 2);
    }
}
