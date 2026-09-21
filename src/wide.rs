//! UTF-16 conversion for the Win32 string APIs.
//!
//! These were three private `to_wide` functions in three modules, all with
//! that name and all with different contracts: one appended a NUL, one
//! appended a NUL *and* truncated to a caller-supplied limit, and one did
//! neither. Reading any single call site told you nothing about which
//! behaviour you were getting, and the failure mode is not a compile error —
//! passing a non-terminated buffer to an API expecting a C string reads past
//! the end of the allocation until it happens to find a zero.
//!
//! The distinction is real and worth keeping, so the fix is not one function
//! but three named for what they actually do. The name now carries the
//! contract.

/// UTF-16 with a terminating NUL, for APIs taking `PCWSTR`.
///
/// The default choice: most Win32 string parameters are C strings and will
/// run off the end of the buffer without this.
pub(crate) fn nul_terminated(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 with no terminator, for APIs taking an explicit length.
///
/// `GetTextExtentPoint32W` and `DrawTextW` take a slice and a count, and a
/// trailing NUL would be measured and drawn as a character.
pub(crate) fn unterminated(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// UTF-16 truncated to `max_chars`, then NUL-terminated.
///
/// For the fixed-size arrays in `NOTIFYICONDATAW`, where the tooltip and
/// balloon fields have hard capacities and the API does not truncate for you.
/// Truncation happens before the NUL so the result always fits in
/// `max_chars + 1`.
///
/// Counts UTF-16 code units, not characters, so a string cut here can in
/// principle split a surrogate pair. Accepted: the alternative is dropping a
/// whole astral character to avoid a lone surrogate that Windows renders as
/// one replacement glyph at the very end of an already-truncated tooltip.
pub(crate) fn truncated(s: &str, max_chars: usize) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().take(max_chars).collect();
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_terminated_ends_with_zero() {
        let v = nul_terminated("ok");
        assert_eq!(v, vec![u16::from(b'o'), u16::from(b'k'), 0]);
    }

    /// The measuring APIs take a count, so a NUL here would be measured and
    /// drawn as a character.
    #[test]
    fn unterminated_has_no_zero() {
        let v = unterminated("ok");
        assert_eq!(v, vec![u16::from(b'o'), u16::from(b'k')]);
        assert!(!v.contains(&0));
    }

    /// Truncation happens before the NUL, so the result fits the fixed-size
    /// field it is copied into.
    #[test]
    fn truncated_fits_the_limit_and_still_terminates() {
        let v = truncated("abcdef", 3);
        assert_eq!(
            v,
            vec![u16::from(b'a'), u16::from(b'b'), u16::from(b'c'), 0]
        );
        assert_eq!(*v.last().unwrap(), 0);
    }

    #[test]
    fn truncated_leaves_short_strings_alone() {
        assert_eq!(
            truncated("ab", 10),
            vec![u16::from(b'a'), u16::from(b'b'), 0]
        );
    }

    /// Non-ASCII must survive: the tooltip carries translated text.
    ///
    /// Two-byte and three-byte UTF-8 sequences in one string, which is what a
    /// translated tooltip is made of. The characters are punctuation and
    /// accents rather than a foreign word, so the fixture states the encoding
    /// property without putting a second language in a source file.
    #[test]
    fn multibyte_text_round_trips() {
        let s = "naïve — 66 °C";
        let v = nul_terminated(s);
        assert_eq!(String::from_utf16_lossy(&v[..v.len() - 1]), s);
    }
}
