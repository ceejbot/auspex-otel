//! Core tracing context types with strong, bug-preventing newtypes.
//!
//! `TraceId` and `SpanId` are intentionally newtypes around fixed-size arrays.
//! This prevents accidental misuse (e.g., passing a trace ID where a span ID is
//! expected) and gives us excellent `Debug`/`Display` representations out of
//! the box.

use std::fmt;
use std::str::FromStr;

use crate::propagation::TraceState;

/// Error returned when parsing a `TraceId` or `SpanId` from hex or bytes.
///
/// Distinguishes the cases required by the W3C / OTEL specs (length, content,
/// and the forbidden all-zero value for generated IDs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Wrong number of characters or bytes.
    InvalidLength,
    /// Non-hex digit encountered.
    InvalidHex,
    /// The all-zero ID, which is invalid per spec for trace/span IDs in
    /// certain contexts (we reject it on parse for generated values too).
    Zero,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => write!(f, "invalid length"),
            Self::InvalidHex => write!(f, "invalid hex digit"),
            Self::Zero => write!(f, "zero ID is not allowed"),
        }
    }
}

impl std::error::Error for ParseError {}

/// A 128-bit trace identifier (W3C traceparent format).
///
/// Stored as `[u8; 16]` internally for efficiency and to avoid allocation.
/// Always treated as an opaque value — never inspect the bytes directly
/// in application code.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; 16]);

impl TraceId {
    /// The all-zero trace ID (invalid per spec, useful as a sentinel).
    // Sentinel + byte constructor: part of the newtype's vocabulary, currently
    // exercised by tests. `as_bytes` (the inverse) is used on the export path.
    #[allow(dead_code)]
    pub const ZERO: Self = Self([0; 16]);

    /// Create a `TraceId` from a 16-byte array.
    #[inline]
    #[allow(dead_code)]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Return the raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Returns true if this is the all-zero (invalid) trace ID.
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }

    /// Generate a fresh 128-bit trace ID using OS randomness.
    ///
    /// Retries until a non-zero value is obtained (per spec, all-zero is
    /// forbidden for trace IDs).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        loop {
            if getrandom::fill(&mut bytes).is_ok() && !bytes.iter().all(|&b| b == 0) {
                return Self(bytes);
            }
            // Extremely unlikely to get all-zero or a getrandom failure;
            // just retry.
        }
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({self})")
    }
}

impl fmt::Display for TraceId {
    /// Lowercase hex, 32 characters, matching W3C/OTEL conventions.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl From<[u8; 16]> for TraceId {
    #[inline]
    fn from(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl TryFrom<&[u8]> for TraceId {
    type Error = &'static str;

    fn try_from(slice: &[u8]) -> Result<Self, Self::Error> {
        if slice.len() != 16 {
            return Err("TraceId must be exactly 16 bytes");
        }
        let mut arr = [0u8; 16];
        arr.copy_from_slice(slice);
        Ok(Self(arr))
    }
}

impl FromStr for TraceId {
    type Err = ParseError;

    /// Parse a 32-character lowercase (or uppercase) hex string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 {
            return Err(ParseError::InvalidLength);
        }
        let mut bytes = [0u8; 16];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_digit(chunk[0]).map_err(|_| ParseError::InvalidHex)?;
            let low = hex_digit(chunk[1]).map_err(|_| ParseError::InvalidHex)?;
            bytes[i] = (high << 4) | low;
        }
        let id = Self(bytes);
        if id.is_zero() {
            return Err(ParseError::Zero);
        }
        Ok(id)
    }
}

#[inline]
const fn hex_digit(c: u8) -> Result<u8, &'static str> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err("invalid hex digit in TraceId"),
    }
}

/// A 64-bit span identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId([u8; 8]);

impl SpanId {
    /// The all-zero span ID (invalid per spec).
    // Sentinel + byte constructor: see the note on `TraceId::ZERO`.
    #[allow(dead_code)]
    pub const ZERO: Self = Self([0; 8]);

    #[inline]
    #[allow(dead_code)]
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }

    #[inline]
    pub fn is_zero(self) -> bool {
        self.0 == [0; 8]
    }

    /// Generate a fresh 64-bit span ID using OS randomness.
    ///
    /// Retries until a non-zero value is obtained (per spec, all-zero is
    /// forbidden).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 8];
        loop {
            if getrandom::fill(&mut bytes).is_ok() && !bytes.iter().all(|&b| b == 0) {
                return Self(bytes);
            }
        }
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({self})")
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl From<[u8; 8]> for SpanId {
    #[inline]
    fn from(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }
}

impl TryFrom<&[u8]> for SpanId {
    type Error = &'static str;

    fn try_from(slice: &[u8]) -> Result<Self, Self::Error> {
        if slice.len() != 8 {
            return Err("SpanId must be exactly 8 bytes");
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(slice);
        Ok(Self(arr))
    }
}

impl FromStr for SpanId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 16 {
            return Err(ParseError::InvalidLength);
        }
        let mut bytes = [0u8; 8];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_digit(chunk[0]).map_err(|_| ParseError::InvalidHex)?;
            let low = hex_digit(chunk[1]).map_err(|_| ParseError::InvalidHex)?;
            bytes[i] = (high << 4) | low;
        }
        let id = Self(bytes);
        if id.is_zero() {
            return Err(ParseError::Zero);
        }
        Ok(id)
    }
}

/// Lightweight context carried across process boundaries (and within the
/// trace).
///
/// Carries the remote (or generated) trace/span IDs plus the sampled decision
/// and any preserved `tracestate` from the inbound `traceparent` / `tracestate`
/// headers.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SpanContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub is_sampled: bool,
    pub trace_state: Option<TraceState>,
}

impl SpanContext {
    // The layer builds `SpanContext` via a struct literal, so these
    // constructors are currently exercised only by the propagation tests. They
    // are the natural builder vocabulary for the type; keep them.
    #[allow(dead_code)]
    pub const fn new(trace_id: TraceId, span_id: SpanId, is_sampled: bool) -> Self {
        Self {
            trace_id,
            span_id,
            is_sampled,
            trace_state: None,
        }
    }

    /// Attach a preserved `tracestate` value.
    #[allow(dead_code)]
    pub fn with_trace_state(mut self, trace_state: TraceState) -> Self {
        self.trace_state = Some(trace_state);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_roundtrip_bytes() {
        let original = TraceId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        let bytes = *original.as_bytes();
        let roundtripped = TraceId::from_bytes(bytes);
        assert_eq!(original, roundtripped);
    }

    #[test]
    fn trace_id_display_and_from_str() {
        let id = TraceId::from_bytes([0x0a; 16]);
        let s = id.to_string();
        assert_eq!(s.len(), 32);
        assert_eq!(s, "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a");

        let parsed: TraceId = s.parse().expect("valid hex");
        assert_eq!(parsed, id);
    }

    #[test]
    fn span_id_roundtrip_and_display() {
        let id = SpanId::from_bytes([0xde, 0xad, 0xbe, 0xef, 0x00, 0x00, 0x00, 0x01]);
        let s = id.to_string();
        assert_eq!(s, "deadbeef00000001");

        let parsed: SpanId = s.parse().expect("valid hex in test");
        assert_eq!(parsed, id);
    }

    #[test]
    fn invalid_lengths_rejected() {
        assert!("1234".parse::<TraceId>().is_err());
        assert!("deadbeef".parse::<SpanId>().is_err());
    }

    #[test]
    fn zero_detection() {
        assert!(TraceId::ZERO.is_zero());
        assert!(!TraceId::from_bytes([1; 16]).is_zero());
        assert!(SpanId::ZERO.is_zero());
    }

    #[test]
    fn generate_never_produces_zero() {
        for _ in 0..100 {
            assert!(!TraceId::generate().is_zero());
            assert!(!SpanId::generate().is_zero());
        }
    }

    #[test]
    fn parse_errors_distinguish_cases() {
        use ParseError::*;
        assert_eq!("".parse::<TraceId>(), Err(InvalidLength));
        assert_eq!("zzzz".parse::<SpanId>(), Err(InvalidLength));
        assert_eq!("gggggggggggggggggggggggggggggggg".parse::<TraceId>(), Err(InvalidHex));
        assert_eq!("00000000000000000000000000000000".parse::<TraceId>(), Err(Zero));
        assert_eq!("0000000000000000".parse::<SpanId>(), Err(Zero));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn trace_id_roundtrips_through_bytes(bytes: [u8; 16]) {
            let id = TraceId::from_bytes(bytes);
            prop_assert_eq!(id.as_bytes(), &bytes);
            prop_assert_eq!(TraceId::from_bytes(*id.as_bytes()), id);
        }

        #[test]
        fn span_id_roundtrips_through_bytes(bytes: [u8; 8]) {
            let id = SpanId::from_bytes(bytes);
            prop_assert_eq!(id.as_bytes(), &bytes);
            prop_assert_eq!(SpanId::from_bytes(*id.as_bytes()), id);
        }

        #[test]
        fn trace_id_roundtrips_through_display_and_parse(bytes: [u8; 16]) {
            // The all-zero ID is rejected on parse by design; skip it here.
            prop_assume!(bytes.iter().any(|&b| b != 0));
            let id = TraceId::from_bytes(bytes);
            let s = id.to_string();
            let parsed: TraceId = s.parse().unwrap();
            prop_assert_eq!(parsed, id);
        }

        #[test]
        fn span_id_roundtrips_through_display_and_parse(bytes: [u8; 8]) {
            prop_assume!(bytes.iter().any(|&b| b != 0));
            let id = SpanId::from_bytes(bytes);
            let s = id.to_string();
            let parsed: SpanId = s.parse().unwrap();
            prop_assert_eq!(parsed, id);
        }
    }
}
