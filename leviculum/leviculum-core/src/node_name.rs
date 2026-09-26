//! The operator-chosen node name: what it may contain, and how long it
//! may be (Codeberg #235/#238).
//!
//! A node has always had a name — `LNode-<hex8>`, derived from its
//! identity so a fresh board is distinguishable from the next one on the
//! bench. This module is the type for the name an operator sets *instead*
//! of that default. It is display only: the identity hash stays the sole
//! addressing and disambiguation mechanism, so two boards may carry the
//! same name and still be told apart everywhere it matters.
//!
//! # Why the length is airtime politics, not cosmetics
//!
//! The name rides in **every** LXMF delivery announce, over LoRa. The
//! announce is `HEADER_MINSIZE(19) + announce_payload_fixed_len(false)
//! =148 + app_data`, and the LXMF `app_data` for a name of `n` bytes is
//! `5 + n` (msgpack: one array header, a two-byte `bin8` header, the
//! name, a nil stamp cost, an empty capability array). So:
//!
//! ```text
//!   announce_bytes(n) = 19 + 148 + 5 + n = 172 + n
//! ```
//!
//! Measured against [`crate::rnode::airtime_ms_with_preamble`]
//! (`leviculum-lxmf/tests/announce_name_airtime.rs` pins every number
//! below):
//!
//! | name | announce | SF7/BW125/CR4:5 | SF12/BW125/CR4:8/preamble-18 |
//! |------|----------|-----------------|------------------------------|
//! | none (management announce, no `app_data`) | 167 B | 272 ms |  9 905 ms |
//! | 14 B (`LNode-<hex8>`, today's default)    | 186 B | 298 ms | 10 953 ms |
//! | 32 B ([`NODE_NAME_MAX_LEN`], the cap)     | 204 B | 323 ms | 11 740 ms |
//! | 64 B (rejected)                           | 236 B | 369 ms | 13 575 ms |
//!
//! Per byte that is 5 payload symbols per 3.5 bytes at SF7 — 1.46 ms —
//! and 8 symbols per 5 bytes at SF12/CR4:8 — 52.4 ms. **A name is not
//! free: at SF12 every character costs a twentieth of a second of the
//! channel, on every announce, forever.**
//!
//! # The chosen cap and what it costs
//!
//! [`NODE_NAME_MAX_LEN`] is **32 bytes**, i.e. 18 bytes above today's
//! derived default. What that buys the operator is a name like
//! `Balkon-Nord-Solarknoten`; what it costs the channel, at the cap:
//!
//! * **+25 ms per announce at SF7**, +787 ms at SF12/CR4:8.
//! * Station cadence (`telemetry-policy`, one report/hour, each preceded
//!   by one delivery announce): **+25 ms/hour at SF7, +0.79 s/hour at
//!   SF12**.
//! * Tracker cadence at its 60 s floor — 60 announces/hour, only a
//!   sane configuration at the fast spreading factors: **+1.5 s/hour at
//!   SF7**.
//!
//! Against the EU 1 % duty cycle (36 s of airtime per hour per band) the
//! worst of those is **4.2 % of the hourly allowance** (SF7, tracker
//! floor) and 2.2 % at SF12 station. That is the price of the cap, paid
//! only by operators who use all of it. Doubling the cap to 64 would
//! double that bill for names nobody reads on a phone screen anyway, and
//! the [`crate::envelope`] frame would still fit — so the bound is a
//! policy decision, not a wire limit, and it is written here.
//!
//! The BLE side has a second, harder bound that is **not** this one:
//! `leviculum_ble_tx::DEVICE_NAME_LEN` (11 bytes, sized against the
//! scan-response PDU and the SoftDevice's default attribute table). A
//! name longer than that is truncated for GAP and stays whole on the
//! mesh; that truncation lives with the constant it depends on, in
//! `leviculum_ble_tx::gap_name`.
//!
//! # Character set: UTF-8, refused rather than trimmed
//!
//! UTF-8 with a **byte** bound, not printable ASCII. An operator in this
//! project's own field deployments names things `Küche` and `Büro`, and
//! a rule that refuses those buys nothing: both surfaces the name
//! reaches are byte-transparent (a msgpack `bin`, a Complete Local Name
//! AD structure) and both consumers (Columba, a phone's Bluetooth list)
//! render UTF-8. The byte bound is what the airtime table above is
//! denominated in, so it is the bound that is enforced; a name of 32
//! Cyrillic characters is refused for being 64 bytes, and the message
//! says so.
//!
//! What is refused, and always with a reason rather than a silent
//! truncation — **a name that arrives different from the one that was
//! typed is worse than an error message**:
//!
//! * invalid UTF-8 (the GAP surface needs a `&str`, and a byte-truncated
//!   codepoint renders as a replacement character or drops the name),
//! * control characters, C0 and C1 and DEL (they would corrupt the
//!   board's own `key=value` log lines and a terminal that prints them),
//! * a leading or trailing space (invisible, and it survives every round
//!   trip to be quietly different from what the operator believes they
//!   set),
//! * the empty name (that is what "clear the name" is for, and it has
//!   its own encoding on the wire).

use core::fmt;

/// The longest node name an operator may set, in **bytes** of UTF-8.
///
/// Derived in the module docs from the announce airtime it costs, not
/// from a buffer size. Changing it changes what every announce costs on
/// the air, so change the table with it.
pub const NODE_NAME_MAX_LEN: usize = 32;

/// The longest name the BLE surfaces can carry, mirrored from
/// `leviculum_ble_tx::DEVICE_NAME_LEN`.
///
/// That crate owns the reasoning — it is sized against the 31-byte legacy
/// scan-response PDU and `BLE_GAP_DEVNAME_DEFAULT_LEN` — and asserts at
/// compile time that the two constants agree, so this copy cannot drift.
/// It lives here because the value is needed off the board too: a host
/// tool's scripted firmware has to shorten a name exactly as the board
/// does, or it proves the host against a device that does not exist.
pub const BLE_NAME_MAX_LEN: usize = 11;

/// The longest prefix of `name` that fits `max_bytes`, obeying two rules
/// a naive `&name[..max_bytes]` breaks.
///
/// 1. **Cut on a codepoint boundary.** The BLE name is handed to the
///    SoftDevice and to the advertisement builder as a `&str`, and a
///    scanner renders it as text; half a codepoint is a replacement
///    character at best and a dropped name at worst. `Küche-Nord` is not
///    allowed to become `Küche-No` plus a broken byte.
/// 2. **Never end on whitespace.** A trailing space is invisible, so a
///    name cut at one reads back as something an operator would compare
///    against their input and find equal — while the two differ by a byte
///    on every surface that stores them.
///
/// Returns `name` (trimmed of trailing whitespace) when it already fits.
#[must_use]
pub fn truncate_on_char_boundary(name: &str, max_bytes: usize) -> &str {
    if name.len() <= max_bytes {
        return name.trim_end();
    }
    let mut end = max_bytes;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].trim_end()
}

/// Why a proposed node name was refused.
///
/// Carries the numbers rather than a bare "invalid": every one of these
/// is shown to a person who has just typed a name and has to know what
/// to type instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeNameError {
    /// No name at all. Clearing the name is a separate command.
    Empty,
    /// Longer than [`NODE_NAME_MAX_LEN`] bytes.
    TooLong {
        /// What was offered, in bytes.
        bytes: usize,
    },
    /// Not valid UTF-8.
    NotUtf8,
    /// Contains a C0/C1 control character or DEL.
    ControlCharacter,
    /// Begins or ends with whitespace.
    Untrimmed,
}

impl fmt::Display for NodeNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(
                f,
                "a node name cannot be empty; clear the name instead to go back to the derived \
                 default"
            ),
            Self::TooLong { bytes } => write!(
                f,
                "a node name may be at most {NODE_NAME_MAX_LEN} bytes of UTF-8 and this one is \
                 {bytes}; the name travels in every announce, so the bound is airtime and not \
                 storage (note that a non-ASCII character costs 2 to 4 bytes)"
            ),
            Self::NotUtf8 => write!(f, "a node name must be valid UTF-8"),
            Self::ControlCharacter => write!(
                f,
                "a node name may not contain control characters; they corrupt the board's own log \
                 lines and are invisible in the name itself"
            ),
            Self::Untrimmed => write!(
                f,
                "a node name may not begin or end with whitespace: it is invisible, and the name \
                 would read back differently from the one that was typed"
            ),
        }
    }
}

/// A validated node name, sized for the wire and for a `static` on the
/// board (no allocator on the firmware side).
///
/// Construct with [`NodeName::parse`] for anything a person or a host
/// tool offers, and with [`NodeName::decode`] for a name read back off a
/// board — see that constructor for why the two rules differ.
#[derive(Clone, Copy)]
pub struct NodeName {
    bytes: [u8; NODE_NAME_MAX_LEN],
    len: u8,
}

impl NodeName {
    /// The empty name: "this surface carries no name".
    ///
    /// Not constructible through [`NodeName::parse`] — an operator
    /// cannot ask for it — but it is what a report says about a surface
    /// that is not carrying one, and it is the total fallback the
    /// firmware uses where a name must never be able to stop a boot.
    pub const EMPTY: Self = Self {
        bytes: [0u8; NODE_NAME_MAX_LEN],
        len: 0,
    };

    /// Validate a name an operator asked for. See the module docs for
    /// every rule and why it exists.
    pub fn parse(raw: &[u8]) -> Result<Self, NodeNameError> {
        if raw.len() > NODE_NAME_MAX_LEN {
            return Err(NodeNameError::TooLong { bytes: raw.len() });
        }
        let text = core::str::from_utf8(raw).map_err(|_| NodeNameError::NotUtf8)?;
        if text.is_empty() {
            return Err(NodeNameError::Empty);
        }
        if text.chars().any(is_control) {
            return Err(NodeNameError::ControlCharacter);
        }
        if text.trim() != text {
            return Err(NodeNameError::Untrimmed);
        }
        Ok(Self::store(raw))
    }

    /// A name read back off a board, or off a flash record.
    ///
    /// Deliberately more permissive than [`NodeName::parse`]: this end of
    /// the wire carries names the board *derived* or *truncated*, not
    /// only names an operator typed, and a decoder that re-applied the
    /// input rules would refuse a legitimate report. It still rejects
    /// what cannot be displayed at all — over-long, non-UTF-8, or
    /// containing a control character.
    ///
    /// The empty name is accepted here and means "this surface carries
    /// no name", which is a thing a report may legitimately say.
    pub fn decode(raw: &[u8]) -> Result<Self, NodeNameError> {
        if raw.len() > NODE_NAME_MAX_LEN {
            return Err(NodeNameError::TooLong { bytes: raw.len() });
        }
        let text = core::str::from_utf8(raw).map_err(|_| NodeNameError::NotUtf8)?;
        if text.chars().any(is_control) {
            return Err(NodeNameError::ControlCharacter);
        }
        Ok(Self::store(raw))
    }

    fn store(raw: &[u8]) -> Self {
        let mut bytes = [0u8; NODE_NAME_MAX_LEN];
        bytes[..raw.len()].copy_from_slice(raw);
        Self {
            bytes,
            // Both constructors bound `raw.len()` by NODE_NAME_MAX_LEN
            // first, and that is far below u8::MAX.
            len: raw.len() as u8,
        }
    }

    /// The name's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// The name as text. Always succeeds: both constructors validate
    /// UTF-8 before storing.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// The name's length in bytes.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// Whether this is the empty name — only reachable through
    /// [`NodeName::decode`], where it means "this surface carries no
    /// name".
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The control characters a name may not contain: C0, DEL, and C1.
///
/// `char::is_control` covers exactly these three ranges; spelled out as
/// a named function so the rule is greppable from the error message.
fn is_control(c: char) -> bool {
    c.is_control()
}

impl fmt::Debug for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeName({:?})", self.as_str())
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for NodeName {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for NodeName {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn parse(text: &str) -> Result<NodeName, NodeNameError> {
        NodeName::parse(text.as_bytes())
    }

    #[test]
    fn an_ordinary_name_round_trips() {
        let name = parse("Balkon-Nord").unwrap();
        assert_eq!(name.as_str(), "Balkon-Nord");
        assert_eq!(name.len(), 11);
        assert_eq!(name.as_bytes(), b"Balkon-Nord");
    }

    #[test]
    fn a_name_at_the_cap_is_accepted_and_one_byte_past_it_is_not() {
        // The bound is bytes, and the boundary itself is the interesting
        // case: an off-by-one here is a name silently refused or an
        // announce silently longer than the airtime table says.
        let at_cap = "x".repeat(NODE_NAME_MAX_LEN);
        assert_eq!(parse(&at_cap).unwrap().len(), NODE_NAME_MAX_LEN);

        let past = "x".repeat(NODE_NAME_MAX_LEN + 1);
        assert_eq!(
            parse(&past),
            Err(NodeNameError::TooLong {
                bytes: NODE_NAME_MAX_LEN + 1
            })
        );
    }

    #[test]
    fn a_german_name_is_accepted_and_counted_in_bytes() {
        // The reason the charset is UTF-8 and not printable ASCII: this
        // project's own field nodes are named like this.
        let name = parse("Küche").unwrap();
        assert_eq!(name.as_str(), "Küche");
        assert_eq!(name.len(), 6, "ü is two bytes and the bound counts bytes");

        // ... and the bound is still bytes, so 17 two-byte characters do
        // not fit where 32 one-byte ones do.
        let long = "ü".repeat(17);
        assert_eq!(long.len(), 34);
        assert_eq!(parse(&long), Err(NodeNameError::TooLong { bytes: 34 }));
    }

    #[test]
    fn the_error_message_says_what_to_type_instead() {
        // The rule these refusals exist for: a name that arrives
        // different from the one that was typed is worse than an error,
        // so the error has to be usable.
        let too_long = parse(&"x".repeat(40)).unwrap_err().to_string();
        assert!(too_long.contains("32"), "{too_long}");
        assert!(too_long.contains("40"), "{too_long}");
        assert!(
            parse("").unwrap_err().to_string().contains("clear"),
            "the empty name has to point at the command that does what was meant"
        );
    }

    #[test]
    fn the_empty_name_is_not_a_name() {
        assert_eq!(parse(""), Err(NodeNameError::Empty));
    }

    #[test]
    fn invalid_utf8_is_refused() {
        // 0xFF is not a legal UTF-8 byte anywhere. The GAP surface needs
        // a &str, and a name that cannot become one has no honest
        // rendering on either surface.
        assert_eq!(NodeName::parse(&[b'a', 0xFF]), Err(NodeNameError::NotUtf8));
        assert_eq!(NodeName::decode(&[b'a', 0xFF]), Err(NodeNameError::NotUtf8));
    }

    #[test]
    fn control_characters_are_refused_on_both_constructors() {
        for raw in ["a\nb", "a\tb", "a\u{7F}b", "a\u{85}b", "\0"] {
            assert_eq!(
                parse(raw),
                Err(NodeNameError::ControlCharacter),
                "{raw:?} was accepted"
            );
            assert_eq!(
                NodeName::decode(raw.as_bytes()),
                Err(NodeNameError::ControlCharacter),
                "{raw:?} was accepted by the decoder"
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_refused_rather_than_trimmed() {
        // Trimming would be the silent-difference failure this whole
        // rule set exists to prevent: the operator would read their name
        // back one character shorter and have no way to know why.
        for raw in [" Balkon", "Balkon ", "\u{a0}Balkon", "Balkon\u{a0}"] {
            assert_eq!(parse(raw), Err(NodeNameError::Untrimmed), "{raw:?}");
        }
        // Interior whitespace is fine — "Balkon Nord" is a name.
        assert_eq!(parse("Balkon Nord").unwrap().as_str(), "Balkon Nord");
    }

    #[test]
    fn the_decoder_accepts_what_a_board_may_honestly_report() {
        // A GAP name truncated to its own bound can end up shorter than
        // anything an operator typed, and the empty name is how a report
        // says "this surface carries no name". Re-applying the input
        // rules here would refuse a legitimate report.
        assert_eq!(NodeName::decode(b"").unwrap().as_str(), "");
        assert!(NodeName::decode(b"").unwrap().is_empty());
        assert_eq!(
            NodeName::decode(b"Balkon Nor").unwrap().as_str(),
            "Balkon Nor"
        );
        // The length bound still holds: a report longer than the cap is
        // a board saying something this firmware has no place to put.
        assert_eq!(
            NodeName::decode(&[b'x'; NODE_NAME_MAX_LEN + 1]),
            Err(NodeNameError::TooLong {
                bytes: NODE_NAME_MAX_LEN + 1
            })
        );
    }

    #[test]
    fn two_names_compare_by_their_bytes_not_by_their_padding() {
        // NodeName carries a fixed-size buffer; equality must not depend
        // on the residue past `len`, or a name set twice would compare
        // unequal to itself.
        let a = parse("ab").unwrap();
        let mut b = parse("abcd").unwrap();
        b = NodeName {
            bytes: b.bytes,
            len: 2,
        };
        assert_eq!(a, b);
        assert_ne!(parse("ab").unwrap(), parse("abc").unwrap());
    }
}
