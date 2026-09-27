//! A small CBOR reader (RFC 8949) for the WebAuthn structures.
//!
//! A ceremony hands the server exactly two CBOR blobs: the attestation object and the COSE
//! credential public key inside the authenticator data. Both use a handful of major types —
//! maps, arrays, byte strings, text strings and integers — so this reader implements those,
//! with definite lengths only. An indefinite-length item is refused rather than guessed at,
//! and so is a floating-point simple value: nothing in WebAuthn carries one, so a blob that
//! does is a blob this server will not read.

/// One CBOR item.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// Major type 0.
    Unsigned(u64),
    /// Major type 1 (the value is the negative integer).
    Negative(i64),
    /// Major type 2.
    Bytes(Vec<u8>),
    /// Major type 3.
    Text(String),
    /// Major type 4.
    Array(Vec<Item>),
    /// Major type 5.
    Map(Vec<(Item, Item)>),
    /// Major type 7, `false`/`true`.
    Bool(bool),
    /// Major type 7, `null`/`undefined`.
    Null,
}

impl Item {
    /// The item as a byte string.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Item::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The item as text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Item::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The item as a signed integer (either sign).
    #[must_use]
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Item::Unsigned(value) => i64::try_from(*value).ok(),
            Item::Negative(value) => Some(*value),
            _ => None,
        }
    }

    /// The item as a map's entries.
    #[must_use]
    pub fn as_entries(&self) -> Option<&[(Item, Item)]> {
        match self {
            Item::Map(entries) => Some(entries),
            _ => None,
        }
    }

    /// Look a key up in a map — the integer labels a COSE key uses.
    #[must_use]
    pub fn map_get(&self, key: i64) -> Option<&Item> {
        self.as_entries()?
            .iter()
            .find(|(entry_key, _)| entry_key.as_integer() == Some(key))
            .map(|(_, value)| value)
    }

    /// Look a key up in a map — the text labels an attestation object uses.
    #[must_use]
    pub fn map_get_text(&self, key: &str) -> Option<&Item> {
        self.as_entries()?
            .iter()
            .find(|(entry_key, _)| entry_key.as_text() == Some(key))
            .map(|(_, value)| value)
    }
}

/// Decode one item that has to cover the whole input.
///
/// # Errors
/// The message names what the blob did that CBOR does not allow.
pub fn decode(bytes: &[u8]) -> Result<Item, String> {
    let mut offset = 0;
    let item = decode_item(bytes, &mut offset)?;
    if offset != bytes.len() {
        return Err(format!(
            "trailing bytes after the item: {} of {} read",
            offset,
            bytes.len()
        ));
    }
    Ok(item)
}

/// Decode one item, advancing the cursor past it.
fn decode_item(bytes: &[u8], offset: &mut usize) -> Result<Item, String> {
    let initial = *bytes.get(*offset).ok_or("the blob ended mid-item")?;
    *offset += 1;

    let major = initial >> 5;
    let info = initial & 0x1f;

    match major {
        0 => {
            let value = length(bytes, offset, info)?;
            Ok(Item::Unsigned(value))
        }
        1 => {
            let value = length(bytes, offset, info)?;
            let value = i64::try_from(value).map_err(|_| "a negative integer overflowed")?;
            Ok(Item::Negative(-1 - value))
        }
        2 => {
            let length = usize::try_from(length(bytes, offset, info)?)
                .map_err(|_| "a byte string was too long")?;
            Ok(Item::Bytes(take(bytes, offset, length)?.to_vec()))
        }
        3 => {
            let length = usize::try_from(length(bytes, offset, info)?)
                .map_err(|_| "a text string was too long")?;
            let raw = take(bytes, offset, length)?;
            let text = std::str::from_utf8(raw)
                .map_err(|_| "a text string was not UTF-8")?
                .to_owned();
            Ok(Item::Text(text))
        }
        4 => {
            let count = usize::try_from(length(bytes, offset, info)?)
                .map_err(|_| "an array was too long")?;
            let mut items = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                items.push(decode_item(bytes, offset)?);
            }
            Ok(Item::Array(items))
        }
        5 => {
            let count =
                usize::try_from(length(bytes, offset, info)?).map_err(|_| "a map was too long")?;
            let mut entries = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                let key = decode_item(bytes, offset)?;
                let value = decode_item(bytes, offset)?;
                entries.push((key, value));
            }
            Ok(Item::Map(entries))
        }
        // A tag wraps one item; nothing here needs the tag itself, so it is skipped and the
        // tagged item answers in its place.
        6 => {
            let _ = length(bytes, offset, info)?;
            decode_item(bytes, offset)
        }
        7 => match info {
            20 => Ok(Item::Bool(false)),
            21 => Ok(Item::Bool(true)),
            22 | 23 => Ok(Item::Null),
            24 => {
                let _simple = take(bytes, offset, 1)?;
                Err("unsupported CBOR simple value".to_owned())
            }
            _ => Err("unsupported CBOR simple value".to_owned()),
        },
        _ => Err(format!("unsupported CBOR major type {major}")),
    }
}

/// Decode one item and report how many bytes it used.
///
/// A COSE key sits inside a longer blob (the authenticator data), so the reader has to say where
/// the item ended and the caller keeps exactly those bytes.
///
/// # Errors
/// The message names what the blob did that CBOR does not allow.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Item, usize), String> {
    let mut offset = 0;
    let item = decode_item(bytes, &mut offset)?;
    Ok((item, offset))
}

/// Read a definite length, or refuse the item.
fn length(bytes: &[u8], offset: &mut usize, info: u8) -> Result<u64, String> {
    match info {
        0..=23 => Ok(u64::from(info)),
        24 => {
            let raw = take(bytes, offset, 1)?;
            Ok(u64::from(raw[0]))
        }
        25 => {
            let raw = take(bytes, offset, 2)?;
            Ok(u64::from(u16::from_be_bytes([raw[0], raw[1]])))
        }
        26 => {
            let raw = take(bytes, offset, 4)?;
            Ok(u64::from(u32::from_be_bytes([
                raw[0], raw[1], raw[2], raw[3],
            ])))
        }
        27 => {
            let raw = take(bytes, offset, 8)?;
            Ok(u64::from_be_bytes([
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            ]))
        }
        _ => Err("an indefinite length is not accepted here".to_owned()),
    }
}

/// Take `count` bytes off the front of the remaining input.
fn take<'a>(bytes: &'a [u8], offset: &mut usize, count: usize) -> Result<&'a [u8], String> {
    let end = offset.saturating_add(count);
    if end > bytes.len() {
        return Err("the blob ended mid-item".to_owned());
    }
    let slice = &bytes[*offset..end];
    *offset = end;
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_types_a_ceremony_uses() {
        // {"fmt":"none","authData":h'0102',"attStmt":{}}
        let mut blob = vec![0xa3];
        blob.extend([0x63, b'f', b'm', b't', 0x64, b'n', b'o', b'n', b'e']);
        blob.extend([0x68]);
        blob.extend(b"authData");
        blob.extend([0x42, 0x01, 0x02]);
        blob.extend([0x67]);
        blob.extend(b"attStmt");
        blob.push(0xa0);

        let item = decode(&blob).expect("a plain attestation object decodes");
        assert_eq!(
            item.map_get_text("fmt").and_then(Item::as_text),
            Some("none")
        );
        assert_eq!(
            item.map_get_text("authData")
                .and_then(Item::as_bytes)
                .map(<[u8]>::to_vec),
            Some(vec![0x01, 0x02])
        );
        assert_eq!(
            item.map_get_text("attStmt")
                .and_then(Item::as_entries)
                .map(<[(Item, Item)]>::len),
            Some(0)
        );
    }

    #[test]
    fn reads_integers_of_either_sign_and_the_cose_labels() {
        // {1: 2, 3: -7, -1: 1, -2: h'00'}
        let mut blob = vec![0xa4];
        blob.extend([0x01, 0x02]);
        blob.extend([0x03, 0x26]);
        blob.extend([0x20, 0x01]);
        blob.extend([0x21, 0x41, 0x00]);

        let item = decode(&blob).expect("a COSE key shape decodes");
        assert_eq!(item.map_get(1).and_then(Item::as_integer), Some(2));
        assert_eq!(item.map_get(3).and_then(Item::as_integer), Some(-7));
        assert_eq!(item.map_get(-1).and_then(Item::as_integer), Some(1));
        assert_eq!(
            item.map_get(-2)
                .and_then(Item::as_bytes)
                .map(<[u8]>::to_vec),
            Some(vec![0x00])
        );
    }

    #[test]
    fn reads_a_long_byte_string_with_a_two_byte_length() {
        let payload = vec![0x5a; 300];
        let mut blob = vec![0x59, 0x01, 0x2c];
        blob.extend_from_slice(&payload);
        let item = decode(&blob).expect("a 300-byte string decodes");
        assert_eq!(item.as_bytes().map(<[u8]>::len), Some(300));
    }

    #[test]
    fn refuses_what_it_should_not_guess_at() {
        assert!(decode(&[0x5f]).is_err(), "indefinite lengths are refused");
        assert!(decode(&[0x00, 0x00]).is_err(), "trailing bytes are refused");
        assert!(decode(&[0x42, 0x01]).is_err(), "a short blob is refused");
        assert!(decode(&[0xf9, 0x00, 0x00]).is_err(), "a float is refused");
    }
}
