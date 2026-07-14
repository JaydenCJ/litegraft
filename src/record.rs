//! SQLite varint and record (serial-type) decoding.
//!
//! litegraft never speaks SQL, but it must read just enough of the record
//! format to walk `sqlite_schema` (page 1) and learn each object's name,
//! type and root page. The decoder below implements the record format from
//! the SQLite file-format specification: a varint header length, a list of
//! varint serial types, then the values.

use std::fmt;

/// Errors produced while decoding low-level SQLite structures.
#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DecodeError {}

fn err<T>(msg: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(msg.into()))
}

/// Decode a SQLite varint (big-endian base-128, at most 9 bytes; the 9th
/// byte contributes all 8 bits). Returns `(value, bytes_consumed)`.
pub fn read_varint(buf: &[u8]) -> Result<(u64, usize), DecodeError> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let Some(&b) = buf.get(i) else {
            return err("truncated varint");
        };
        if i == 8 {
            // Ninth byte: all 8 bits are payload.
            value = (value << 8) | b as u64;
            return Ok((value, 9));
        }
        value = (value << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    unreachable!("loop covers all 9 bytes");
}

/// A decoded record value. litegraft only distinguishes what it needs:
/// integers (root pages), text (names, SQL) and "everything else".
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// Byte length of the value encoded by a serial type.
pub fn serial_type_len(serial: u64) -> Result<usize, DecodeError> {
    Ok(match serial {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => return err(format!("reserved serial type {serial}")),
        n if n >= 12 && n % 2 == 0 => ((n - 12) / 2) as usize,
        n => ((n - 13) / 2) as usize,
    })
}

fn read_be_int(buf: &[u8]) -> i64 {
    // Sign-extend a 1..8-byte big-endian two's-complement integer.
    let mut v: i64 = if buf[0] & 0x80 != 0 { -1 } else { 0 };
    for &b in buf {
        v = (v << 8) | b as i64;
    }
    v
}

fn decode_value(serial: u64, buf: &[u8]) -> Result<Value, DecodeError> {
    let len = serial_type_len(serial)?;
    if buf.len() < len {
        return err(format!(
            "record value truncated (serial {serial}, need {len} bytes)"
        ));
    }
    let raw = &buf[..len];
    Ok(match serial {
        0 => Value::Null,
        1..=6 => Value::Int(read_be_int(raw)),
        7 => Value::Float(f64::from_bits(u64::from_be_bytes(raw.try_into().unwrap()))),
        8 => Value::Int(0),
        9 => Value::Int(1),
        n if n >= 12 && n % 2 == 0 => Value::Blob(raw.to_vec()),
        _ => Value::Text(String::from_utf8_lossy(raw).into_owned()),
    })
}

/// Decode a full record payload into its column values.
pub fn decode_record(payload: &[u8]) -> Result<Vec<Value>, DecodeError> {
    let (header_len, n) = read_varint(payload)?;
    let header_len = header_len as usize;
    if header_len < n || header_len > payload.len() {
        return err(format!("record header length {header_len} out of range"));
    }
    let mut header = &payload[n..header_len];
    let mut body = &payload[header_len..];
    let mut values = Vec::new();
    while !header.is_empty() {
        let (serial, used) = read_varint(header)?;
        header = &header[used..];
        let value = decode_value(serial, body)?;
        body = &body[serial_type_len(serial)?..];
        values.push(value);
    }
    Ok(values)
}

/// Encode a varint (used by tests and fixture builders; kept here so encode
/// and decode stay in one reviewed place).
pub fn write_varint(mut value: u64) -> Vec<u8> {
    if value <= 0x7f {
        return vec![value as u8];
    }
    if value > 0x00ff_ffff_ffff_ffff {
        // Needs the 9-byte form: 8 continuation bytes + full trailing byte.
        let mut out = Vec::with_capacity(9);
        let tail = (value & 0xff) as u8;
        value >>= 8;
        let mut groups = [0u8; 8];
        for g in groups.iter_mut().rev() {
            *g = (value & 0x7f) as u8;
            value >>= 7;
        }
        for g in groups {
            out.push(g | 0x80);
        }
        out.push(tail);
        return out;
    }
    let mut groups = Vec::new();
    while value > 0 {
        groups.push((value & 0x7f) as u8);
        value >>= 7;
    }
    let mut out = Vec::with_capacity(groups.len());
    for (i, g) in groups.iter().rev().enumerate() {
        if i + 1 == groups.len() {
            out.push(*g);
        } else {
            out.push(g | 0x80);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_short_encodings() {
        assert_eq!(read_varint(&[0x00]).unwrap(), (0, 1));
        assert_eq!(read_varint(&[0x7f]).unwrap(), (127, 1));
        // 0x81 0x00 = 128; 0xff 0x7f = 16383 (max 2-byte value).
        assert_eq!(read_varint(&[0x81, 0x00]).unwrap(), (128, 2));
        assert_eq!(read_varint(&[0xff, 0x7f]).unwrap(), (16383, 2));
    }

    #[test]
    fn varint_nine_byte_form_uses_all_bits_of_last_byte() {
        // u64::MAX encodes as 9 bytes; the last byte carries 8 bits.
        let buf = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(read_varint(&buf).unwrap(), (u64::MAX, 9));
    }

    #[test]
    fn varint_truncated_is_an_error_not_a_panic() {
        assert!(read_varint(&[]).is_err());
        assert!(read_varint(&[0x81]).is_err());
    }

    #[test]
    fn varint_roundtrip_across_length_boundaries() {
        // Every encoding-length boundary, both sides.
        let cases = [
            0,
            1,
            127,
            128,
            16383,
            16384,
            2097151,
            2097152,
            268435455,
            268435456,
            u32::MAX as u64,
            0x00ff_ffff_ffff_ffff,
            0x0100_0000_0000_0000,
            u64::MAX,
        ];
        for v in cases {
            let enc = write_varint(v);
            assert_eq!(read_varint(&enc).unwrap(), (v, enc.len()), "value {v}");
        }
    }

    #[test]
    fn serial_type_lengths_match_spec_table() {
        for (serial, want) in [
            (0u64, 0usize),
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 4),
            (5, 6),
            (6, 8),
            (7, 8),
            (8, 0),
            (9, 0),
        ] {
            assert_eq!(serial_type_len(serial).unwrap(), want, "serial {serial}");
        }
        assert_eq!(serial_type_len(12).unwrap(), 0); // empty blob
        assert_eq!(serial_type_len(13).unwrap(), 0); // empty string
        assert_eq!(serial_type_len(18).unwrap(), 3); // 3-byte blob
        assert_eq!(serial_type_len(19).unwrap(), 3); // 3-byte text
        assert!(serial_type_len(10).is_err(), "10 and 11 are reserved");
    }

    #[test]
    fn decode_record_with_mixed_types() {
        // Record: header_len, serials [text(3), int8, null], body "abc", 0x05.
        let payload = [0x04, 19, 0x01, 0x00, b'a', b'b', b'c', 0x05];
        let values = decode_record(&payload).unwrap();
        assert_eq!(
            values,
            vec![Value::Text("abc".into()), Value::Int(5), Value::Null]
        );
    }

    #[test]
    fn decode_record_numeric_serials() {
        // Serial types 8 and 9 encode 0 and 1 with no body bytes at all.
        let payload = [0x03, 8, 9];
        assert_eq!(
            decode_record(&payload).unwrap(),
            vec![Value::Int(0), Value::Int(1)]
        );
        // -2 as a 1-byte twos-complement int (serial 1) must sign-extend.
        let payload = [0x02, 1, 0xfe];
        assert_eq!(decode_record(&payload).unwrap(), vec![Value::Int(-2)]);
        // -1 as a 4-byte int (serial 4).
        let payload = [0x02, 4, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(decode_record(&payload).unwrap(), vec![Value::Int(-1)]);
        // Serial 7 is a big-endian IEEE 754 double.
        let mut payload = vec![0x02, 7];
        payload.extend_from_slice(&1.5f64.to_bits().to_be_bytes());
        assert_eq!(decode_record(&payload).unwrap(), vec![Value::Float(1.5)]);
    }

    #[test]
    fn decode_record_blob_vs_text_parity_rule() {
        // Serial 14 = 1-byte blob, serial 15 = 1-byte text.
        let payload = [0x03, 14, 15, 0xaa, b'x'];
        let values = decode_record(&payload).unwrap();
        assert_eq!(
            values,
            vec![Value::Blob(vec![0xaa]), Value::Text("x".into())]
        );
    }

    #[test]
    fn decode_record_truncated_body_is_an_error() {
        // Header claims a 3-byte text but only 1 body byte follows.
        let payload = [0x02, 19, b'a'];
        assert!(decode_record(&payload).is_err());
    }

    #[test]
    fn decode_record_header_len_beyond_payload_is_an_error() {
        let payload = [0x7f, 1, 2];
        assert!(decode_record(&payload).is_err());
    }

    #[test]
    fn value_accessors_pick_only_matching_variants() {
        assert_eq!(Value::Int(7).as_int(), Some(7));
        assert_eq!(Value::Text("t".into()).as_int(), None);
        assert_eq!(Value::Text("t".into()).as_text(), Some("t"));
        assert_eq!(Value::Null.as_text(), None);
    }
}
