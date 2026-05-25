//! Pure W3C `traceparent` header parser and formatter (v0.1 supports only
//! version `00`).
//!
//! This module is intentionally small and allocation-free on the happy path
//! (except for the `Display` / `to_string` formatter used in tests and future
//! outbound propagation).

use std::fmt;

use crate::context::{ParseError as IdParseError, SpanId, TraceId};

/// Error type for `traceparent` header parsing.
///
/// Keeps the ID-level errors precise while adding header-specific cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Not exactly `version-traceid-parentid-flags` separated by `-`.
    InvalidFormat,
    /// First field was not the supported version "00".
    UnsupportedVersion,
    /// The trace-id segment was invalid (length, hex, or zero).
    InvalidTraceId(IdParseError),
    /// The parent-id segment was invalid (length, hex, or zero).
    InvalidParentId(IdParseError),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat => write!(f, "invalid traceparent format"),
            Self::UnsupportedVersion => write!(f, "unsupported traceparent version (only 00 is supported in v0.1)"),
            Self::InvalidTraceId(e) => write!(f, "invalid trace-id: {e}"),
            Self::InvalidParentId(e) => write!(f, "invalid parent-id: {e}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Compact representation of the trace-flags field (last 2 hex characters).
///
/// In v0.1 we only act on the sampled bit (0x01). Unknown bits are ignored on
/// parse (per the spec) and the formatter always emits a clean 00/01 byte so
/// the sampled decision is correctly round-tripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraceFlags(u8);

impl TraceFlags {
    /// The sampled bit as defined by the W3C Trace Context specification.
    pub const SAMPLED: u8 = 0b0000_0001;

    /// Construct from a raw flags byte (any value is accepted; only the
    /// sampled bit is observed by this crate in v0.1).
    #[inline]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Construct a flags value that only carries the sampled decision
    /// (used by the formatter for outbound headers in v0.1).
    #[inline]
    pub const fn from_sampled(sampled: bool) -> Self {
        Self(if sampled { Self::SAMPLED } else { 0 })
    }

    /// Returns true when the sampled bit is set.
    #[inline]
    pub const fn is_sampled(&self) -> bool {
        (self.0 & Self::SAMPLED) != 0
    }

    /// Raw byte value (useful for tests and future richer flag handling).
    #[inline]
    pub const fn as_u8(&self) -> u8 {
        self.0
    }
}

/// Parsed representation of a `traceparent` header value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceParent {
    pub trace_id: TraceId,
    pub parent_id: SpanId,
    pub flags: TraceFlags,
}

/// Parse a `traceparent` header value (the part after the header name).
///
/// Only version `00` is accepted. All-zero IDs are rejected (via the existing
/// `TraceId`/`SpanId` parsers). Unknown flag bits are accepted but ignored
/// except for the sampled decision.
pub fn parse_traceparent(s: &str) -> Result<TraceParent, ParseError> {
    let mut parts = s.split('-');

    let version = parts.next().ok_or(ParseError::InvalidFormat)?;
    if version != "00" {
        return Err(ParseError::UnsupportedVersion);
    }

    let trace_id_str = parts.next().ok_or(ParseError::InvalidFormat)?;
    let parent_id_str = parts.next().ok_or(ParseError::InvalidFormat)?;
    let flags_str = parts.next().ok_or(ParseError::InvalidFormat)?;

    // Must be exactly four segments
    if parts.next().is_some() {
        return Err(ParseError::InvalidFormat);
    }

    // Trace and parent IDs are validated by the strong ID parsers (length,
    // hex, zero rejection).
    let trace_id = trace_id_str.parse::<TraceId>().map_err(ParseError::InvalidTraceId)?;
    let parent_id = parent_id_str.parse::<SpanId>().map_err(ParseError::InvalidParentId)?;

    // Flags: exactly two hex characters.
    if flags_str.len() != 2 {
        return Err(ParseError::InvalidFormat);
    }
    let mut flag_byte = 0u8;
    for (i, c) in flags_str.as_bytes().iter().enumerate() {
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err(ParseError::InvalidFormat),
        };
        let shift = if i == 0 { 4 } else { 0 };
        flag_byte |= digit << shift;
    }

    Ok(TraceParent {
        trace_id,
        parent_id,
        flags: TraceFlags::from_bits(flag_byte),
    })
}

/// Format a `TraceParent` back into the canonical lowercase W3C string.
///
/// The flags byte is emitted as two lowercase hex characters. In v0.1 we only
/// ever set the sampled bit (or clear it), satisfying the "roundtrip the
/// sampled decision" requirement while ignoring unknown bits on input.
impl fmt::Display for TraceParent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Always emit clean 00/01 per the documented behavior (unknown bits on
        // input are ignored; only the sampled decision is round-tripped).
        let clean = TraceFlags::from_sampled(self.flags.is_sampled());
        write!(f, "00-{}-{}-{:02x}", self.trace_id, self.parent_id, clean.as_u8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{SpanId, TraceId};

    // ---------------------------------------------------------------------
    // Unit tests for the malformed-header rejection cases required by W3C
    // ---------------------------------------------------------------------

    #[test]
    fn rejects_wrong_number_of_segments() {
        assert!(parse_traceparent("00-abc").is_err());
        assert!(parse_traceparent("00-abc-def").is_err());
        assert!(parse_traceparent("00-abc-def-01-extra").is_err());
    }

    #[test]
    fn rejects_unsupported_version() {
        assert!(matches!(
            parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            Err(ParseError::UnsupportedVersion)
        ));
        assert!(matches!(
            parse_traceparent("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            Err(ParseError::UnsupportedVersion)
        ));
    }

    #[test]
    fn rejects_invalid_trace_id_via_id_parser() {
        // Too short, bad hex, or zero (the ID parser already rejects zero)
        assert!(matches!(
            parse_traceparent("00-short-00f067aa0ba902b7-01"),
            Err(ParseError::InvalidTraceId(_))
        ));
        assert!(matches!(
            parse_traceparent("00-zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-00f067aa0ba902b7-01"),
            Err(ParseError::InvalidTraceId(_))
        ));
        assert!(matches!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            Err(ParseError::InvalidTraceId(IdParseError::Zero))
        ));
    }

    #[test]
    fn rejects_invalid_parent_id_via_id_parser() {
        assert!(matches!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-short-01"),
            Err(ParseError::InvalidParentId(_))
        ));
        assert!(matches!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"),
            Err(ParseError::InvalidParentId(IdParseError::Zero))
        ));
    }

    #[test]
    fn rejects_bad_flags_length() {
        assert!(parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1").is_err());
        assert!(parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-010").is_err());
    }

    #[test]
    fn rejects_non_hex_flags() {
        assert!(parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-gg").is_err());
    }

    // ---------------------------------------------------------------------
    // Happy-path parsing + sampled flag extraction
    // ---------------------------------------------------------------------

    #[test]
    fn parses_canonical_w3c_example() {
        // Famous example from the W3C Trace Context specification
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let tp = parse_traceparent(header).expect("valid traceparent");

        assert_eq!(tp.trace_id.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tp.parent_id.to_string(), "00f067aa0ba902b7");
        assert!(tp.flags.is_sampled());
    }

    #[test]
    fn parses_unsampled_flag() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
        let tp = parse_traceparent(header).expect("traceparent should be parsed");
        assert!(!tp.flags.is_sampled());
    }

    #[test]
    fn accepts_uppercase_hex_in_ids_and_flags() {
        let header = "00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01";
        let tp = parse_traceparent(header).expect("case-insensitive hex");
        assert!(tp.flags.is_sampled());
    }

    // ---------------------------------------------------------------------
    // Formatter produces lowercase W3C-compliant strings
    // ---------------------------------------------------------------------

    #[test]
    fn formatter_emits_lowercase_and_correct_flags() {
        let trace = "4bf92f3577b34da6a3ce929d0e0e4736"
            .parse::<TraceId>()
            .expect("trace id should be parsed");
        let parent = "00f067aa0ba902b7".parse::<SpanId>().expect("spanid should be parsed");

        let sampled = TraceParent {
            trace_id: trace,
            parent_id: parent,
            flags: TraceFlags::from_sampled(true),
        };
        assert_eq!(
            sampled.to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );

        let unsampled = TraceParent {
            trace_id: trace,
            parent_id: parent,
            flags: TraceFlags::from_sampled(false),
        };
        assert_eq!(
            unsampled.to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"
        );
    }

    #[test]
    fn formatter_cleans_unknown_flag_bits() {
        // Even if a parsed TraceParent carries extra flag bits (allowed on input),
        // the Display must emit only the clean sampled decision (00 or 01).
        let trace = "4bf92f3577b34da6a3ce929d0e0e4736"
            .parse::<TraceId>()
            .expect("tradeid should be parsed");
        let parent = "00f067aa0ba902b7".parse::<SpanId>().expect("spanid should be parsed");

        let dirty_sampled = TraceParent {
            trace_id: trace,
            parent_id: parent,
            flags: TraceFlags::from_bits(0b0000_0011), // sampled + unknown bit
        };
        assert_eq!(
            dirty_sampled.to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );

        let dirty_unsampled = TraceParent {
            trace_id: trace,
            parent_id: parent,
            flags: TraceFlags::from_bits(0b0000_0010), // only unknown bit
        };
        assert_eq!(
            dirty_unsampled.to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"
        );
    }

    // ---------------------------------------------------------------------
    // Property-based roundtrip tests (the key Validate requirement)
    // ---------------------------------------------------------------------

    #[cfg(test)]
    mod proptests {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #[test]
            fn traceparent_roundtrips(
                // Generate non-zero IDs (the generators from Task 1.1 already guarantee this)
                trace_bytes in any::<[u8; 16]>(),
                parent_bytes in any::<[u8; 8]>(),
                sampled in any::<bool>(),
            ) {
                // Skip the extremely rare all-zero cases (they are rejected by ID parsers)
                if trace_bytes.iter().all(|&b| b == 0) || parent_bytes.iter().all(|&b| b == 0) {
                    return Ok(());
                }

                let trace_id = TraceId::from_bytes(trace_bytes);
                let parent_id = SpanId::from_bytes(parent_bytes);
                let flags = TraceFlags::from_sampled(sampled);

                let original = TraceParent { trace_id, parent_id, flags };
                let serialized = original.to_string();
                let reparsed = parse_traceparent(&serialized).expect("roundtrip must succeed");

                prop_assert_eq!(reparsed.trace_id, original.trace_id);
                prop_assert_eq!(reparsed.parent_id, original.parent_id);
                prop_assert_eq!(reparsed.flags.is_sampled(), original.flags.is_sampled());
            }
        }
    }
}
