/// A fsverity digest stored as bare hex (no prefix). Used consistently throughout
/// the migration pipeline. Call sites choose `as_hex()` (directories, files) or
/// `as_prefixed()` (.origin, .imginfo, composefs= cmdline via the matching
/// accessor) so the `sha512:` prefix never leaks into path names or kernel args.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VerityDigest(String);

impl VerityDigest {
    /// Create from a bare hex digest. Panics if the string contains a colon or
    /// non-hex characters — this is deliberately strict so callers don't
    /// accidentally pass the prefixed form.
    pub fn from_hex(hex: &str) -> Self {
        assert!(
            !hex.contains(':'),
            "VerityDigest::from_hex called with prefixed string: {hex}"
        );
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "VerityDigest::from_hex called with non-hex string: {hex}"
        );
        Self(hex.to_string())
    }

    /// Parse a digest string that may or may not have a `sha512:` prefix.
    /// Strips the prefix if present.
    pub fn from_prefixed_or_hex(s: &str) -> Self {
        let hex = s.strip_prefix("sha512:").unwrap_or(s);
        Self::from_hex(hex)
    }

    /// Bare hex (no prefix) — used for file/directory names.
    pub fn as_hex(&self) -> &str {
        &self.0
    }

    /// Prefixed form `sha512:<hex>` — used in .origin, .imginfo, and other
    /// places that require the canonical composefs digest wire format.
    pub fn as_prefixed(&self) -> String {
        format!("sha512:{}", self.0)
    }
}

impl std::fmt::Display for VerityDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TDD tests for VerityDigest.

    #[test]
    fn verity_digest_from_hex_bare() {
        let d = VerityDigest::from_hex("abc123def456");
        assert_eq!(d.as_hex(), "abc123def456");
    }

    #[test]
    fn verity_digest_as_prefixed() {
        let d = VerityDigest::from_hex("abc123def456");
        assert_eq!(d.as_prefixed(), "sha512:abc123def456");
    }

    #[test]
    fn verity_digest_from_prefixed_string() {
        let d = VerityDigest::from_prefixed_or_hex("sha512:abc123def456");
        assert_eq!(d.as_hex(), "abc123def456");
    }

    #[test]
    fn verity_digest_from_bare_string() {
        let d = VerityDigest::from_prefixed_or_hex("abc123def456");
        assert_eq!(d.as_hex(), "abc123def456");
    }

    #[test]
    #[should_panic(expected = "prefixed")]
    fn verity_digest_from_hex_rejects_colon() {
        VerityDigest::from_hex("sha512:abc");
    }

    #[test]
    #[should_panic(expected = "non-hex")]
    fn verity_digest_from_hex_rejects_non_hex() {
        VerityDigest::from_hex("xyz");
    }

    #[test]
    fn verity_digest_display_is_bare_hex() {
        let d = VerityDigest::from_hex("abc123");
        assert_eq!(format!("{d}"), "abc123");
    }

    #[test]
    fn verity_digest_first_two_chars_are_prefix() {
        // Used for object store directory naming: objects/xx/rest
        let d = VerityDigest::from_hex("abcdef1234567890");
        assert_eq!(&d.as_hex()[..2], "ab");
        assert_eq!(&d.as_hex()[2..], "cdef1234567890");
    }
}
