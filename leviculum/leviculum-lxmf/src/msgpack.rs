use alloc::vec::Vec;
use rmp::{
    decode::{
        bytes::{Bytes, BytesReadError},
        NumValueReadError, ValueReadError,
    },
    Marker,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Type,
    Overflow,
    Trailing,
    /// Container nesting deeper than `MAX_SKIP_DEPTH`.
    Depth,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "msgpack value is truncated"),
            Self::Type => write!(f, "unexpected msgpack type"),
            Self::Overflow => write!(f, "msgpack value does not fit its target type"),
            Self::Trailing => write!(f, "trailing bytes after the msgpack value"),
            Self::Depth => write!(f, "msgpack container nesting is too deep"),
        }
    }
}

impl core::error::Error for Error {}

/// Coarse wire type used to decode protocol unions without exposing `rmp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Nil,
    False,
    True,
    Array,
    Float,
    Other,
}

pub fn array(out: &mut Vec<u8>, n: usize) {
    let _ = rmp::encode::write_array_len(out, n as u32);
}
pub fn map(out: &mut Vec<u8>, n: usize) {
    let _ = rmp::encode::write_map_len(out, n as u32);
}
pub fn bin(out: &mut Vec<u8>, v: &[u8]) {
    let _ = rmp::encode::write_bin(out, v);
}
pub fn string(out: &mut Vec<u8>, value: &str) {
    let _ = rmp::encode::write_str(out, value);
}
pub fn f64(out: &mut Vec<u8>, v: f64) {
    let _ = rmp::encode::write_f64(out, v);
}
pub fn uint(out: &mut Vec<u8>, v: u64) {
    let _ = rmp::encode::write_uint(out, v);
}
pub fn int(out: &mut Vec<u8>, v: i64) {
    if v >= 0 {
        uint(out, v as u64);
    } else {
        let _ = rmp::encode::write_sint(out, v);
    }
}
pub fn nil(out: &mut Vec<u8>) {
    let _ = rmp::encode::write_nil(out);
}
pub fn bool(out: &mut Vec<u8>, value: bool) {
    let _ = rmp::encode::write_bool(out, value);
}
/// Append one already-validated MessagePack value without changing its wire type.
pub fn append_raw(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(value);
}

fn take<'a>(d: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8], Error> {
    let e = p.checked_add(n).ok_or(Error::Overflow)?;
    let v = d.get(*p..e).ok_or(Error::Truncated)?;
    *p = e;
    Ok(v)
}

fn take_u16(d: &[u8], p: &mut usize) -> Result<u16, Error> {
    let bytes = take(d, p, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn take_u32(d: &[u8], p: &mut usize) -> Result<u32, Error> {
    let bytes = take(d, p, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn map_value_error(error: ValueReadError<BytesReadError>) -> Error {
    match error {
        ValueReadError::InvalidMarkerRead(_) | ValueReadError::InvalidDataRead(_) => {
            Error::Truncated
        }
        ValueReadError::TypeMismatch(_) => Error::Type,
    }
}

fn map_number_error(error: NumValueReadError<BytesReadError>) -> Error {
    match error {
        NumValueReadError::InvalidMarkerRead(_) | NumValueReadError::InvalidDataRead(_) => {
            Error::Truncated
        }
        NumValueReadError::TypeMismatch(_) => Error::Type,
        NumValueReadError::OutOfRange => Error::Overflow,
    }
}

/// Inspect the next MessagePack value kind without consuming it.
pub fn peek_kind(d: &[u8], p: usize) -> Result<Kind, Error> {
    let marker = d
        .get(p)
        .copied()
        .map(Marker::from_u8)
        .ok_or(Error::Truncated)?;
    Ok(match marker {
        Marker::Null => Kind::Nil,
        Marker::False => Kind::False,
        Marker::True => Kind::True,
        Marker::FixArray(_) | Marker::Array16 | Marker::Array32 => Kind::Array,
        Marker::F32 | Marker::F64 => Kind::Float,
        _ => Kind::Other,
    })
}

pub fn array_len(d: &[u8], p: &mut usize) -> Result<usize, Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let n = rmp::decode::read_array_len(&mut rd).map_err(map_value_error)?;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    Ok(n as usize)
}
pub fn map_len(d: &[u8], p: &mut usize) -> Result<usize, Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let n = rmp::decode::read_map_len(&mut rd).map_err(map_value_error)?;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    Ok(n as usize)
}
pub fn read_bin<'a>(d: &'a [u8], p: &mut usize) -> Result<&'a [u8], Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let n = rmp::decode::read_bin_len(&mut rd).map_err(map_value_error)? as usize;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    take(d, p, n)
}
pub fn read_str<'a>(d: &'a [u8], p: &mut usize) -> Result<&'a str, Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let n = rmp::decode::read_str_len(&mut rd).map_err(map_value_error)? as usize;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    core::str::from_utf8(take(d, p, n)?).map_err(|_| Error::Type)
}
pub fn read_f64(d: &[u8], p: &mut usize) -> Result<f64, Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let value = rmp::decode::read_f64(&mut rd).map_err(map_value_error)?;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    Ok(value)
}
/// One MessagePack number together with the family the wire used for it.
///
/// Callers that only need the value take [`read_number_f64`]. The family
/// matters where a decoded value has to be packed again in the form the
/// reference's `msgpack.packb` would produce: an int goes back as a
/// minimal-width int, a float as float64.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Int(v) => v as f64,
            Self::Float(v) => v,
        }
    }
}

/// Read any MessagePack numeric representation, keeping its family.
///
/// Non-numeric markers give [`Error::Type`]; an unsigned value above
/// `i64::MAX` gives [`Error::Overflow`] rather than a silently rounded double.
pub fn read_number(d: &[u8], p: &mut usize) -> Result<Number, Error> {
    match d.get(*p).copied().ok_or(Error::Truncated)? {
        0xca => {
            let marker = take(d, p, 1)?[0];
            debug_assert_eq!(marker, 0xca);
            let bits = f32::from_be_bytes(take(d, p, 4)?.try_into().map_err(|_| Error::Truncated)?);
            Ok(Number::Float(bits as f64))
        }
        0xcb => read_f64(d, p).map(Number::Float),
        _ => read_int(d, p).map(Number::Int),
    }
}

/// Read any MessagePack numeric representation as `f64`.
pub fn read_number_f64(d: &[u8], p: &mut usize) -> Result<f64, Error> {
    read_number(d, p).map(Number::as_f64)
}
pub fn read_bool(d: &[u8], p: &mut usize) -> Result<bool, Error> {
    match take(d, p, 1)?[0] {
        0xc2 => Ok(false),
        0xc3 => Ok(true),
        _ => Err(Error::Type),
    }
}
pub fn read_nil(d: &[u8], p: &mut usize) -> Result<(), Error> {
    if take(d, p, 1)?[0] == 0xc0 {
        Ok(())
    } else {
        Err(Error::Type)
    }
}
pub fn read_uint(d: &[u8], p: &mut usize) -> Result<u64, Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let value = rmp::decode::read_int::<u64, _>(&mut rd).map_err(map_number_error)?;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    Ok(value)
}
pub fn read_int(d: &[u8], p: &mut usize) -> Result<i64, Error> {
    let mut rd = Bytes::new(d.get(*p..).ok_or(Error::Truncated)?);
    let value = rmp::decode::read_int::<i64, _>(&mut rd).map_err(map_number_error)?;
    *p = p
        .checked_add(rd.position() as usize)
        .ok_or(Error::Overflow)?;
    Ok(value)
}
pub fn raw<'a>(d: &'a [u8], p: &mut usize) -> Result<&'a [u8], Error> {
    let s = *p;
    skip(d, p)?;
    Ok(&d[s..*p])
}
/// Maximum msgpack container nesting depth accepted by [`skip`].
///
/// Bounds the recursion so a maliciously deep nested container in untrusted
/// wire bytes cannot overflow the stack (an abort, not a catchable panic).
/// `skip` runs on unauthenticated input: `Message::unpack` skips unknown
/// field values before the Ed25519 signature is checked, and the propagation
/// decoder skips values straight off the wire. LXMF's own field/metadata
/// payloads nest only a couple of levels, so 64 is far above any legitimate
/// use.
///
/// Same value as `leviculum-core`'s `resource::msgpack::MAX_SKIP_DEPTH`, which
/// bounds the identical operation for resource advertisements. Keeping the two
/// identical means one number to reason about rather than two nearly-equal ones.
const MAX_SKIP_DEPTH: usize = 64;

/// Skip a single msgpack value at the current position.
///
/// Nesting is capped at `MAX_SKIP_DEPTH`: a container nested deeper returns
/// [`Error::Depth`] rather than recursing until the stack overflows.
pub fn skip(d: &[u8], p: &mut usize) -> Result<(), Error> {
    skip_depth(d, p, MAX_SKIP_DEPTH)
}

/// Depth-limited body of [`skip`]. `depth` is the remaining nesting budget;
/// each container level recurses with `depth - 1`, and a container found at
/// `depth == 0` is rejected.
fn skip_depth(d: &[u8], p: &mut usize, depth: usize) -> Result<(), Error> {
    let marker = Marker::from_u8(take(d, p, 1)?[0]);
    match marker {
        Marker::FixPos(_) | Marker::FixNeg(_) | Marker::Null | Marker::False | Marker::True => {
            Ok(())
        }
        Marker::FixMap(n) => {
            let inner = depth.checked_sub(1).ok_or(Error::Depth)?;
            for _ in 0..n {
                skip_depth(d, p, inner)?;
                skip_depth(d, p, inner)?
            }
            Ok(())
        }
        Marker::FixArray(n) => {
            let inner = depth.checked_sub(1).ok_or(Error::Depth)?;
            for _ in 0..n {
                skip_depth(d, p, inner)?
            }
            Ok(())
        }
        Marker::FixStr(n) => {
            take(d, p, n as usize)?;
            Ok(())
        }
        Marker::Bin8 | Marker::Str8 => {
            let n = take(d, p, 1)?[0] as usize;
            take(d, p, n)?;
            Ok(())
        }
        Marker::Bin16 | Marker::Str16 => {
            let n = take_u16(d, p)? as usize;
            take(d, p, n)?;
            Ok(())
        }
        Marker::Bin32 | Marker::Str32 => {
            let n = take_u32(d, p)? as usize;
            take(d, p, n)?;
            Ok(())
        }
        Marker::F32 | Marker::U32 | Marker::I32 => {
            take(d, p, 4)?;
            Ok(())
        }
        Marker::F64 | Marker::U64 | Marker::I64 => {
            take(d, p, 8)?;
            Ok(())
        }
        Marker::U8 | Marker::I8 => {
            take(d, p, 1)?;
            Ok(())
        }
        Marker::U16 | Marker::I16 => {
            take(d, p, 2)?;
            Ok(())
        }
        Marker::Array16 => {
            let n = take_u16(d, p)?;
            let inner = depth.checked_sub(1).ok_or(Error::Depth)?;
            for _ in 0..n {
                skip_depth(d, p, inner)?
            }
            Ok(())
        }
        Marker::Array32 => {
            let n = take_u32(d, p)?;
            let inner = depth.checked_sub(1).ok_or(Error::Depth)?;
            for _ in 0..n {
                skip_depth(d, p, inner)?
            }
            Ok(())
        }
        Marker::Map16 => {
            let n = take_u16(d, p)?;
            let inner = depth.checked_sub(1).ok_or(Error::Depth)?;
            for _ in 0..n {
                skip_depth(d, p, inner)?;
                skip_depth(d, p, inner)?
            }
            Ok(())
        }
        Marker::Map32 => {
            let n = take_u32(d, p)?;
            let inner = depth.checked_sub(1).ok_or(Error::Depth)?;
            for _ in 0..n {
                skip_depth(d, p, inner)?;
                skip_depth(d, p, inner)?
            }
            Ok(())
        }
        Marker::FixExt1 => skip_ext(d, p, 1),
        Marker::FixExt2 => skip_ext(d, p, 2),
        Marker::FixExt4 => skip_ext(d, p, 4),
        Marker::FixExt8 => skip_ext(d, p, 8),
        Marker::FixExt16 => skip_ext(d, p, 16),
        Marker::Ext8 => {
            let n = take(d, p, 1)?[0] as usize;
            skip_ext(d, p, n)
        }
        Marker::Ext16 => {
            let n = take_u16(d, p)? as usize;
            skip_ext(d, p, n)
        }
        Marker::Ext32 => {
            let n = take_u32(d, p)? as usize;
            skip_ext(d, p, n)
        }
        Marker::Reserved => Err(Error::Type),
    }
}

fn skip_ext(d: &[u8], p: &mut usize, payload_len: usize) -> Result<(), Error> {
    take(d, p, payload_len.checked_add(1).ok_or(Error::Overflow)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (Codeberg #263, item 1): a deeply nested container must NOT
    /// recurse until the stack overflows. `skip` reads unauthenticated wire
    /// bytes — `Message::unpack` skips unknown field values before the Ed25519
    /// signature check, and the propagation decoder skips straight off the
    /// wire — so a chain of fixarray-len-1 tags (`0x91 0x91 ...`) bought one
    /// stack frame per input byte and aborted the process (a remote DoS).
    ///
    /// The assertion is on the typed error, never on the abort: a test that
    /// actually overflows takes the whole test binary with it.
    #[test]
    fn skip_rejects_deeply_nested_container() {
        let mut buf = alloc::vec![0x91u8; 100_000];
        buf.push(0x90); // innermost: empty array
        let mut pos = 0;
        assert_eq!(skip(&buf, &mut pos), Err(Error::Depth));
    }

    /// The cap fires on the first level past the limit. Cheap counterpart to
    /// the 100k-deep case above: this one is red on unbounded code by
    /// returning `Ok`, rather than by aborting, so it can be run against the
    /// pre-fix behaviour safely.
    #[test]
    fn skip_rejects_nesting_one_past_the_limit() {
        let mut buf = alloc::vec![0x91u8; MAX_SKIP_DEPTH + 1];
        buf.push(0xc0); // innermost value: nil
        let mut pos = 0;
        assert_eq!(skip(&buf, &mut pos), Err(Error::Depth));
    }

    /// A container nested exactly at the depth limit is still accepted, so the
    /// cap does not reject legitimate (shallow) payloads.
    #[test]
    fn skip_accepts_nesting_at_depth_limit() {
        let mut buf = alloc::vec![0x91u8; MAX_SKIP_DEPTH];
        buf.push(0xc0); // innermost value: nil
        let mut pos = 0;
        skip(&buf, &mut pos).expect("nesting at the limit is accepted");
        assert_eq!(pos, buf.len(), "fully consumed");
    }

    /// The budget is per nesting level, not per skipped value: a wide map of
    /// shallow values must not exhaust it.
    #[test]
    fn skip_budget_is_per_level_not_per_value() {
        let mut buf = alloc::vec![0x8fu8]; // fixmap, 15 entries
        for i in 0..15u8 {
            buf.push(0xa1); // fixstr len 1 (key)
            buf.push(b'k');
            buf.push(0x91); // fixarray len 1 (value)
            buf.push(i); // positive fixint
        }
        let mut pos = 0;
        skip(&buf, &mut pos).expect("a wide but shallow container is accepted");
        assert_eq!(pos, buf.len(), "fully consumed");
    }

    /// `raw` shares `skip`'s bound, so the depth cap also covers the
    /// field-collecting decode paths (`Message::decode_payload`,
    /// `propagation` metadata) that use it.
    #[test]
    fn raw_inherits_the_depth_bound() {
        let mut buf = alloc::vec![0x91u8; 100_000];
        buf.push(0x90);
        let mut pos = 0;
        assert_eq!(raw(&buf, &mut pos).err(), Some(Error::Depth));
    }
}
