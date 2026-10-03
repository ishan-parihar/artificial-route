//! The two encodings the envelope and the key file need.
//!
//! `base64` is already in the workspace's dependency graph
//! (`Cargo.lock: base64 0.22.1`) and does the real work. Hex is 15 lines here
//! rather than a second dependency: it is needed only to accept
//! `openssl rand -hex 32` key files and to print a salt, and a hand-rolled
//! encoder is easier to audit than a crate for something this small.
//!
//! // ponytail: URL-safe base64 with no padding, so an envelope is safe in a URL
//! query string (`docs/04` allows token-in-URL for legacy clients) and a length
//! check on a field is exact — 12 bytes is always 16 characters, 16 bytes is
//! always 22. Standard base64 would put `+` and `/` in a credential and `/` in
//! a path segment. Costs ~12% size, which for a 32-byte key is 4 characters.

/// Lowercase hex.
pub(crate) mod hex {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    /// Encodes `bytes` as lowercase hex.
    #[must_use]
    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(char::from(DIGITS[usize::from(b >> 4)]));
            out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
        }
        out
    }

    /// Decodes hex, accepting either case. `None` on any non-hex character or an
    /// odd length.
    #[must_use]
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let bytes = s.as_bytes();
        let pairs = bytes.as_chunks::<2>();
        if !pairs.1.is_empty() {
            // A trailing odd byte: `as_chunks` hands it back separately rather
            // than dropping it, which would silently decode to one byte short.
            return None;
        }
        let mut out = Vec::with_capacity(pairs.0.len());
        for [hi, lo] in pairs.0 {
            out.push((nibble(*hi)? << 4) | nibble(*lo)?);
        }
        Some(out)
    }

    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{decode, encode};

        #[test]
        fn round_trips() {
            let bytes = [0_u8, 1, 15, 16, 200, 255];
            assert_eq!(decode(&encode(&bytes)), Some(bytes.to_vec()));
        }

        #[test]
        fn decodes_uppercase() {
            assert_eq!(decode("DEADbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        }

        #[test]
        fn rejects_odd_length_and_non_hex() {
            assert!(decode("abc").is_none());
            assert!(decode("zz").is_none());
        }
    }
}

/// URL-safe base64, no padding.
pub(crate) mod b64 {
    use base64::Engine as _;
    pub use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    /// Encodes `bytes`.
    #[must_use]
    pub fn encode(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Decodes, or `None` on malformed input.
    #[must_use]
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        URL_SAFE_NO_PAD.decode(s).ok()
    }

    /// Encoded length of `n` raw bytes. Exact, and used for envelope field
    /// validation before a decode is attempted.
    ///
    /// `ceil(n * 4 / 3)`, not `ceil(n / 3) * 4`: the latter is wrong for every
    /// length that is not a multiple of three, and a 16-byte tag encodes to 22
    /// characters rather than 24.
    #[must_use]
    pub const fn encoded_len(n: usize) -> usize {
        (n * 4).div_ceil(3)
    }

    #[cfg(test)]
    mod tests {
        use base64::Engine as _;

        use super::{URL_SAFE_NO_PAD, encoded_len};

        #[test]
        fn encoded_len_matches_the_encoder_for_the_envelope_field_sizes() {
            for n in [0_usize, 1, 3, 12, 16, 17, 32, 64] {
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(vec![0_u8; n]).len(),
                    encoded_len(n),
                    "n={n}"
                );
            }
        }
    }
}

/// Serde helper so [`crate::Salt`] persists as base64 rather than as a JSON
/// array of 16 numbers.
pub(crate) mod b64_salt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::hash::SALT_LEN;

    /// Serializes the salt as base64.
    pub fn serialize<S: Serializer>(salt: &[u8; SALT_LEN], s: S) -> Result<S::Ok, S::Error> {
        super::b64::encode(salt).serialize(s)
    }

    /// Deserializes a base64 salt of exactly the right length.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; SALT_LEN], D::Error> {
        let raw = String::deserialize(d)?;
        let bytes = super::b64::decode(&raw)
            .ok_or_else(|| serde::de::Error::custom("salt is not base64"))?;
        bytes.try_into().map_err(|v: Vec<u8>| {
            let n = v.len();
            serde::de::Error::custom(format!("salt must be {SALT_LEN} bytes, got {n}"))
        })
    }
}
