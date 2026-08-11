//! Fractional ordering keys — the reason moving a card is one write.
//!
//! Every child of every container in this crate carries a [`Rank`]: a base-62
//! fraction, stored as a string, whose **byte order is its numeric order**. A card's
//! position is that key, not an array index, and the whole point is what *doesn't*
//! happen when the board changes:
//!
//! - Dropping a card between two others writes one key. With integer positions it
//!   renumbers every card below it, and `docs/01-architecture.md` §4 already made
//!   this decision once for z-order — in a CRDT each of those renumberings is an
//!   operation to store, undo and merge.
//! - Moving a card to another column writes one key and one column id. Nothing in
//!   either column is touched, so two cards that were adjacent before the move are
//!   still adjacent, still in the same order, with byte-identical keys.
//!
//! `vellum-doc` gets its z-order keys from Loro's own fractional index. This crate
//! has no Loro — it is pure logic that has to unit-test without a document — so the
//! same idea is spelled out here, in eight functions, over an alphabet chosen so
//! that `str`'s `Ord` *is* the ordering. There is no comparison function to remember
//! to call: sorting a `Vec<Rank>` sorts it correctly.
//!
//! # The canonical form
//!
//! A rank is a non-empty string of base-62 digits, read as the fraction `0.d₁d₂…`,
//! and **never ends in the zero digit**. That last rule is what makes the encoding
//! injective: `"1"` and `"10"` are the same number, and if both could exist then two
//! cards could have different keys, equal values, and no defined order between them.
//! Every function here preserves it, and [`Rank::try_from`] enforces it on the way
//! in from a file.
//!
//! # Digits
//!
//! `0-9`, `A-Z`, `a-z` — in ASCII order, so digit value and byte value increase
//! together and lexicographic comparison of the encoded strings agrees with the
//! comparison of the fractions they denote.
//!
//! # Key length
//!
//! Appending to the end of a list lengthens the key by one digit roughly every 30
//! appends, so a column that has had 1,000 cards appended one after another ends
//! with ~34-byte keys. That is deliberate and it is fine here: these containers hold
//! tens to hundreds of children. The unbounded fix is the integer-part scheme from
//! the fractional-indexing literature (`a0`, `a1`, … `b00`), which is more code than
//! it is worth today and would not change this API.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The number base. 62 rather than 10 because a fatter alphabet is what keeps keys
/// short: each digit of a base-62 key holds as much ordering room as 1.8 decimal
/// digits, at the same one byte on disk.
const BASE: u8 = 62;

/// The digit used when there is no neighbour to bias towards — a first key, or the
/// first digit of an extension. Centred, so that the next insertion has as much room
/// below it as above.
const MID: u8 = BASE / 2;

/// A child's position among its siblings: a base-62 fraction in `(0, 1)`.
///
/// Ordering is the derived lexicographic order on the encoded string, which is
/// exactly the order of the fractions — see the module docs for why that holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Rank(String);

impl Rank {
    /// A key between two neighbours, either of which may be absent to mean "the
    /// start of the list" or "the end of it".
    ///
    /// The result is strictly between the two, so the caller's insert lands exactly
    /// where the neighbours say and nothing else moves.
    ///
    /// If the pair arrives out of order — `before >= after`, which means the caller
    /// is working from a list that has since changed under it — `after` is ignored
    /// and the key lands after `before`. A stale drag preview should place the card
    /// somewhere sane, not panic the app mid-drop.
    pub fn between(before: Option<&Self>, after: Option<&Self>) -> Self {
        let (before, after) = match (before, after) {
            (Some(low), Some(high)) if low >= high => (Some(low), None),
            pair => pair,
        };
        let digits = match (before, after) {
            (None, None) => vec![MID],
            (Some(low), None) => next_after(&low.digits()),
            (None, Some(high)) => prev_before(&high.digits()),
            (Some(low), Some(high)) => between_digits(&low.digits(), &high.digits()),
        };
        Self(encode(&digits))
    }

    /// The key for the only child of an empty container.
    pub fn first() -> Self {
        Self::between(None, None)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn digits(&self) -> Vec<u8> {
        self.0.bytes().map(|b| digit_value(b).expect("a Rank is validated on construction")).collect()
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Rank> for String {
    fn from(rank: Rank) -> Self {
        rank.0
    }
}

impl TryFrom<String> for Rank {
    type Error = RankError;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        match validate(&key) {
            Ok(()) => Ok(Self(key)),
            Err(reason) => Err(RankError { key, reason }),
        }
    }
}

impl FromStr for Rank {
    type Err = RankError;

    fn from_str(key: &str) -> Result<Self, Self::Err> {
        Self::try_from(key.to_owned())
    }
}

/// A string that is not a valid [`Rank`].
///
/// Only ever produced when reading a key from outside this crate — a saved board, a
/// hand-written test. Ranks generated here are correct by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankError {
    /// The offending key, quoted back so a corrupt board says which one.
    pub key: String,
    pub reason: RankProblem,
}

/// Which of the canonical form's three rules the key broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankProblem {
    Empty,
    /// Contains a byte outside `0-9A-Za-z`.
    NotADigit,
    /// Ends in the zero digit, so it names a fraction some shorter key also names.
    /// See the module docs: allowing both would leave two children with no order
    /// between them.
    TrailingZero,
}

impl fmt::Display for RankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            RankProblem::Empty => "it is empty",
            RankProblem::NotADigit => "it contains a non-base-62 byte",
            RankProblem::TrailingZero => "it ends in the zero digit",
        };
        write!(f, "{:?} is not a valid rank: {reason}", self.key)
    }
}

impl std::error::Error for RankError {}

fn validate(key: &str) -> Result<(), RankProblem> {
    let bytes = key.as_bytes();
    let Some((&last, _)) = bytes.split_last() else {
        return Err(RankProblem::Empty);
    };
    if bytes.iter().any(|&b| digit_value(b).is_none()) {
        return Err(RankProblem::NotADigit);
    }
    if last == b'0' {
        return Err(RankProblem::TrailingZero);
    }
    Ok(())
}

fn digit_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'Z' => Some(byte - b'A' + 10),
        b'a'..=b'z' => Some(byte - b'a' + 36),
        _ => None,
    }
}

fn digit_byte(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        10..=35 => b'A' + value - 10,
        _ => b'a' + value - 36,
    }
}

fn encode(digits: &[u8]) -> String {
    debug_assert!(!digits.is_empty() && *digits.last().unwrap() != 0, "{digits:?} is not canonical");
    digits.iter().map(|&d| digit_byte(d) as char).collect()
}

/// The smallest canonical key greater than `a`. `a` empty means "greater than
/// nothing", i.e. the first key in a list.
///
/// Incrementing the last digit that has room, and dropping everything after it, is
/// what keeps appends cheap: `V` → `W`, `Vz` → `W`, and only when every digit is
/// exhausted does the key grow — `zz` → `zzV`.
fn next_after(a: &[u8]) -> Vec<u8> {
    match a.iter().rposition(|&d| d + 1 < BASE) {
        Some(i) => {
            let mut digits = a[..=i].to_vec();
            digits[i] += 1;
            digits
        }
        None => {
            let mut digits = a.to_vec();
            digits.push(MID);
            digits
        }
    }
}

/// The largest canonical key smaller than `b`, which must be canonical and
/// non-empty.
fn prev_before(b: &[u8]) -> Vec<u8> {
    let mut digits = b.to_vec();
    let last = digits.len() - 1;
    debug_assert!(digits[last] != 0, "a canonical key never ends in zero");
    if digits[last] >= 2 {
        digits[last] -= 1;
    } else {
        // Decrementing a final `1` to `0` would break the canonical form, so borrow
        // instead: `…1` becomes `…0V`, which sits between `…0` and `…1`.
        digits[last] = 0;
        digits.push(MID);
    }
    digits
}

/// A canonical key strictly between two canonical keys, `a < b`.
fn between_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    // Digits the two already agree on cannot influence the answer, so emit them and
    // work on what is left. Digits past the end of `a` read as zero, which is what
    // the fraction says they are.
    let mut shared = 0;
    while shared < b.len() && a.get(shared).copied().unwrap_or(0) == b[shared] {
        shared += 1;
    }
    // `a < b` guarantees the scan stops inside `b`: running to the end of `b` would
    // mean every digit of `b` equals the corresponding digit of `a`, which makes
    // `b <= a`.
    debug_assert!(shared < b.len(), "between_digits requires a < b");
    let mut out = b[..shared].to_vec();
    let a = a.get(shared..).unwrap_or(&[]);
    let b = &b[shared..];

    let low = a.first().copied().unwrap_or(0);
    let high = b[0];
    debug_assert!(high > low);

    if high - low >= 2 {
        // Room at this digit, so the key needs no more of them.
        out.push((low + high) / 2);
        return out;
    }

    // The digits are consecutive: nothing fits between them here, and the key has to
    // get longer. Two ways to do that, and which is shorter depends on the tails —
    // stay on `a`'s digit and go above its tail, or step up to `b`'s digit and go
    // below its tail. Key length is the only thing a fractional index spends, so
    // both are built and the shorter wins.
    let mut stay = out.clone();
    stay.push(low);
    stay.extend_from_slice(&next_after(a.get(1..).unwrap_or(&[])));

    if b.len() > 1 {
        let mut step = out;
        step.push(high);
        step.extend_from_slice(&prev_before(&b[1..]));
        if step.len() < stay.len() {
            return step;
        }
        return stay;
    }
    stay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(key: &str) -> Rank {
        key.parse().expect("test key should be valid")
    }

    /// The property the whole crate rests on. Everything else in this module is an
    /// implementation of it.
    fn assert_between(before: Option<&Rank>, after: Option<&Rank>) -> Rank {
        let mid = Rank::between(before, after);
        validate(mid.as_str()).unwrap_or_else(|e| panic!("{mid} is not canonical: {e:?}"));
        if let Some(b) = before {
            assert!(*b < mid, "{b} < {mid} does not hold");
        }
        if let Some(a) = after {
            assert!(mid < *a, "{mid} < {a} does not hold");
        }
        mid
    }

    #[test]
    fn a_first_key_sits_in_the_middle_so_both_ends_have_room() {
        let first = assert_between(None, None);
        assert_eq!(first.as_str(), "V");
        assert_between(None, Some(&first));
        assert_between(Some(&first), None);
    }

    #[test]
    fn appending_increments_the_last_digit_that_has_room() {
        assert_eq!(Rank::between(Some(&rank("V")), None).as_str(), "W");
        assert_eq!(Rank::between(Some(&rank("Vz")), None).as_str(), "W");
        assert_eq!(Rank::between(Some(&rank("zz")), None).as_str(), "zzV");
    }

    #[test]
    fn prepending_borrows_rather_than_writing_a_trailing_zero() {
        assert_eq!(Rank::between(None, Some(&rank("V"))).as_str(), "U");
        assert_eq!(Rank::between(None, Some(&rank("1"))).as_str(), "0V");
        assert_eq!(Rank::between(None, Some(&rank("01"))).as_str(), "00V");
    }

    #[test]
    fn a_key_between_two_neighbours_splits_the_gap() {
        assert_eq!(Rank::between(Some(&rank("1")), Some(&rank("3"))).as_str(), "2");
        // Consecutive digits: the key has to grow, and it grows by one digit.
        let mid = assert_between(Some(&rank("1")), Some(&rank("2")));
        assert_eq!(mid.as_str(), "1V");
        // …unless stepping up to the upper key's digit is shorter.
        assert_eq!(Rank::between(Some(&rank("1zz")), Some(&rank("2V"))).as_str(), "2U");
    }

    /// A shared prefix is emitted untouched, so keys agree digit for digit until the
    /// point where they have to differ.
    #[test]
    fn common_prefixes_survive() {
        let mid = assert_between(Some(&rank("VVV1")), Some(&rank("VVV3")));
        assert_eq!(mid.as_str(), "VVV2");
    }

    /// The adversarial case: always insert into the same gap. The keys must stay
    /// correct however long they get, which is the thing a naive midpoint on `f64`
    /// gets wrong after about 50 insertions.
    #[test]
    fn repeatedly_splitting_the_same_gap_never_loses_the_ordering() {
        let low = rank("1");
        let mut high = rank("2");
        let mut seen = vec![low.clone(), high.clone()];
        for _ in 0..200 {
            high = assert_between(Some(&low), Some(&high));
            seen.push(high.clone());
        }
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "every key generated must be distinct");
    }

    /// Sequential appends are the common case, and the module docs claim a digit per
    /// ~30 of them. This is that claim, asserted.
    #[test]
    fn a_thousand_appends_stay_short_and_stay_ordered() {
        let mut keys = vec![Rank::first()];
        for _ in 0..1000 {
            let next = Rank::between(keys.last(), None);
            assert!(*keys.last().unwrap() < next);
            keys.push(next);
        }
        let longest = keys.iter().map(|k| k.as_str().len()).max().unwrap();
        assert!(longest <= 40, "1000 appends produced a {longest}-byte key");
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    /// Byte order is the ordering. If this ever stopped holding, every container in
    /// the crate would silently reorder its children.
    #[test]
    fn string_order_is_fraction_order_across_the_whole_alphabet() {
        let mut keys: Vec<Rank> = Vec::new();
        for value in 1..BASE {
            keys.push(rank(&(digit_byte(value) as char).to_string()));
        }
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        // A key that extends another is greater than it: `V` < `V1` < `W`.
        assert!(rank("V") < rank("V1"));
        assert!(rank("V1") < rank("W"));
    }

    #[test]
    fn malformed_keys_are_refused_with_the_key_in_the_message() {
        assert_eq!("".parse::<Rank>().unwrap_err().reason, RankProblem::Empty);
        assert_eq!("V-W".parse::<Rank>().unwrap_err().reason, RankProblem::NotADigit);
        let err = "V0".parse::<Rank>().unwrap_err();
        assert_eq!(err.reason, RankProblem::TrailingZero);
        assert!(err.to_string().contains("V0"), "{err}");
    }

    /// Out-of-order neighbours come from a drag preview computed against a list that
    /// has since changed. That must not panic, and the card must still land
    /// somewhere defined.
    #[test]
    fn a_misordered_pair_places_after_the_lower_bound_rather_than_panicking() {
        let placed = Rank::between(Some(&rank("W")), Some(&rank("V")));
        assert!(placed > rank("W"));
        let equal = Rank::between(Some(&rank("V")), Some(&rank("V")));
        assert!(equal > rank("V"));
    }

    #[test]
    fn ranks_round_trip_through_their_string_form() {
        let key = Rank::between(Some(&rank("1")), Some(&rank("2")));
        let text = String::from(key.clone());
        assert_eq!(text.parse::<Rank>().unwrap(), key);
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, format!("\"{key}\""));
        assert_eq!(serde_json::from_str::<Rank>(&json).unwrap(), key);
        assert!(serde_json::from_str::<Rank>("\"V0\"").is_err(), "serde must enforce the form too");
    }
}
