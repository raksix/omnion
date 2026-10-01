//! Base64url without padding, the encoding every Web Push value travels in.
//!
//! **A hand-rolled decoder, not a dependency.** The platform already depends on `base64`,
//! but only inside crates that need it for something else, and pulling the crate into
//! `crates/core` — which is infrastructure for everything — to decode a 32-byte key would
//! make the whole workspace inherit a dependency for one function. The alphabet below is
//! RFC 4648 §5; padding is *rejected* rather than skipped, because a padded VAPID key is a
//! value an operator pasted from the wrong tool and it should say so.
//!
//! Every function returns `None` for malformed input rather than a partial decode. A
//! half-decoded key is the worst outcome available: it is 32 bytes of somebody's key that
//! sign correctly and match nothing.

/// The URL-safe alphabet, in value order.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Decode base64url (no padding). `None` for padding, whitespace, or an unknown character.
#[must_use]
pub fn decode(input: &str) -> Option<Vec<u8>> {
    // A length of 1 modulo 4 cannot be produced by any byte sequence: it would mean the last
    // group has one character, which encodes nothing. Rejecting it here means the tail loop
    // below never has to reason about a group it cannot fill.
    if input.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 2);
    for chunk in input.as_bytes().chunks(4) {
        let mut bits = 0u32;
        let mut taken = 0;
        for byte in chunk {
            let value = value_of(*byte)? as u32;
            bits = (bits << 6) | value;
            taken += 1;
        }
        // Re-pad with zeros, then keep only the `taken * 6` significant bits.
        bits <<= 6 * (4 - taken);
        match taken {
            2 => out.push((bits >> 16) as u8),
            3 => {
                out.push((bits >> 16) as u8);
                out.push((bits >> 8) as u8);
            }
            4 => {
                out.push((bits >> 16) as u8);
                out.push((bits >> 8) as u8);
                out.push(bits as u8);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Encode base64url without padding.
#[must_use]
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut bits = 0u32;
        for index in 0..3 {
            bits = (bits << 8) | u32::from(chunk.get(index).copied().unwrap_or(0));
        }
        let significant = chunk.len() * 8;
        out.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        out.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        if significant > 8 {
            out.push(ALPHABET[(bits >> 6) as usize & 63] as char);
        }
        if significant > 16 {
            out.push(ALPHABET[bits as usize & 63] as char);
        }
    }
    out
}

/// The value of one alphabet character, or `None` when it is not in the alphabet.
fn value_of(byte: u8) -> Option<u8> {
    // Written as a search rather than a match on purpose: `=` must be *rejected*, not
    // skipped, and a table lookup that maps it to zero would silently accept a padded value.
    ALPHABET.iter().position(|c| *c == byte).map(|i| i as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors_round_trip() {
        // RFC 4648 §10: the same alphabet, with and without padding.
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");

        for sample in [b"".as_slice(), b"f", b"fo", b"foo", b"foob", b"foobar"] {
            assert_eq!(decode(&encode(sample)).as_deref(), Some(sample));
        }
    }

    #[test]
    fn the_url_safe_alphabet_differs_from_the_standard_one() {
        // `+` and `/` are the two values RFC 4648 §5 replaces. If the decoder ever accepted
        // them, a standard-alphabet key would decode into bytes nobody's key is made of.
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(decode("-_8").as_deref(), Some(&[0xfbu8, 0xffu8][..]));
        assert!(decode("+/8").is_none());
    }

    #[test]
    fn padding_and_whitespace_are_refused_rather_than_skipped() {
        assert!(decode("Zg==").is_none(), "padding must not be accepted");
        assert!(
            decode("Zm9v YmFy").is_none(),
            "whitespace must not be accepted"
        );
        assert!(decode("Zm9v\n").is_none());
    }

    #[test]
    fn a_length_that_encodes_nothing_is_refused() {
        assert!(decode("A").is_none());
        assert!(decode("Zm9vY").is_none());
    }

    #[test]
    fn every_byte_round_trips_through_both_alphabets() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).as_deref(), Some(all.as_slice()));
    }

    #[test]
    fn a_thirty_two_byte_key_is_exactly_thirty_two_bytes_back() {
        let key: Vec<u8> = (0..32u8).collect();
        let encoded = encode(&key);
        assert_eq!(
            encoded.len(),
            43,
            "32 bytes is 43 base64url characters, unpadded"
        );
        assert_eq!(decode(&encoded).as_deref(), Some(key.as_slice()));
    }
}
