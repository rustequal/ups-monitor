//! Order-preserving INI reader/writer. Unknown keys and comments survive a
//! round trip, as required for the config file.

#[derive(Debug, Clone, Default)]
pub(crate) struct Ini {
    lines: Vec<Line>,
}

#[derive(Debug, Clone)]
enum Line {
    Blank(String),
    Section {
        /// Lowercased name, used for matching.
        name: String,
        /// The line as the user wrote it, written back verbatim. Lookups are
        /// case-insensitive, as INI files conventionally are, but a round trip
        /// must not be a rewrite: storing only the lowercased name silently
        /// turned a hand-written `[General]` into `[general]` on the first
        /// settings change.
        raw: String,
    },
    Pair {
        section: String,
        /// Lowercased key, used for matching.
        key: String,
        /// Everything up to and including the `=`, written back verbatim: the
        /// line's indentation and the key as the user cased it. The same rule
        /// the section header follows, and for the same reason — matching is
        /// case-insensitive, but a round trip must not be a rewrite. Storing
        /// only the lowercased key turned a hand-written `Language = ru` into
        /// `language = en` the first time anything else in the file changed,
        /// and dropped whatever indentation the line carried.
        prefix: String,
        /// Everything after the `=`, verbatim. [`Ini::set`] replaces this and
        /// nothing else.
        value: String,
    },
}

impl Ini {
    /// Parses INI text. UTF-8 BOM is stripped if present; locale files are
    /// specified as BOM-less but tolerating one costs nothing.
    pub(crate) fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = Vec::new();
        let mut section = String::new();

        for raw in text.lines() {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
                lines.push(Line::Blank(raw.to_owned()));
            } else if trimmed.starts_with('[') {
                // A line opening with `[` is a section header or it is
                // nothing. It used to fall through to the `=` branch when the
                // bracket was left unclosed, so `[device` became a key named
                // `[device` and `[a]=b` a key named `[a]` — silently, in a
                // file that is edited by hand, where an unclosed bracket is an
                // ordinary typo rather than an exotic input. Malformed headers
                // are preserved verbatim as `Blank`, like every other line
                // this parser does not understand, so rewriting the file does
                // not destroy what the user typed.
                if let Some(name) = trimmed.strip_suffix(']') {
                    section = name[1..].trim().to_lowercase();
                    lines.push(Line::Section {
                        name: section.clone(),
                        raw: raw.to_owned(),
                    });
                } else {
                    lines.push(Line::Blank(raw.to_owned()));
                }
            } else if let Some(eq) = raw.find('=') {
                // Split on the raw line, not the trimmed one: the leading
                // whitespace belongs to the prefix, and it cannot contain an
                // `=` to shift the split.
                let (prefix, value) = raw.split_at(eq + 1);
                lines.push(Line::Pair {
                    section: section.clone(),
                    key: raw[..eq].trim().to_lowercase(),
                    prefix: prefix.to_owned(),
                    value: value.to_owned(),
                });
            } else {
                lines.push(Line::Blank(raw.to_owned()));
            }
        }
        Self { lines }
    }

    pub(crate) fn get(&self, section: &str, key: &str) -> Option<&str> {
        let section = section.to_lowercase();
        let key = key.to_lowercase();
        self.lines.iter().find_map(|l| match l {
            Line::Pair {
                section: s,
                key: k,
                value,
                ..
            } if *s == section && *k == key => Some(value.trim()),
            _ => None,
        })
    }

    pub(crate) fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.get(section, key)?.to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    pub(crate) fn get_u32(&self, section: &str, key: &str) -> Option<u32> {
        parse_u32(self.get(section, key)?)
    }

    pub(crate) fn get_i32(&self, section: &str, key: &str) -> Option<i32> {
        parse_i32(self.get(section, key)?)
    }

    /// Replaces a value in place, or appends it to the section, creating the
    /// section if needed. Untouched lines keep their original text.
    pub(crate) fn set(&mut self, section: &str, key: &str, value: &str) {
        let section_l = section.to_lowercase();
        let key_l = key.to_lowercase();

        for line in &mut self.lines {
            if let Line::Pair {
                section: s,
                key: k,
                value: v,
                ..
            } = line
            {
                if *s == section_l && *k == key_l {
                    // Only the value, and only the part of it that is the
                    // value. The prefix carries the user's spelling of the key
                    // and the line's indentation; the gap after the `=` is the
                    // same kind of fact about how this line was written, so it
                    // is kept as well. Writing a space unconditionally turned a
                    // hand-written `a=1` into `a= 2` — spaced on one side of
                    // the `=` and not the other, which is not a convention but
                    // a typo, in a file people open in an editor. A line keeps
                    // its shape; a line this call creates gets the shape used
                    // below.
                    let gap: String = v.chars().take_while(|c| c.is_whitespace()).collect();
                    *v = format!("{gap}{value}");
                    return;
                }
            }
        }

        // "The end of the first matching section", said once: `find_map` stops
        // at that section instead of building an iterator over every match and
        // taking one element from it.
        let insert_at = self.lines.iter().enumerate().find_map(|(i, l)| {
            if !matches!(l, Line::Section { name, .. } if *name == section_l) {
                return None;
            }
            // The section's body: everything after its header up to the next
            // one. Both edges are found by searching that slice rather than by
            // walking a cursor over the whole file — the two loops this
            // replaces each carried their own bound (`end < len` on the way
            // out, `end > i + 1` on the way back), and a section that is the
            // last in the file or holds nothing but blanks is exactly where
            // one of those bounds used to have to be right.
            let body = self.lines.get(i + 1..).unwrap_or_default();
            let len = body
                .iter()
                .position(|l| matches!(l, Line::Section { .. }))
                .unwrap_or(body.len());
            // Trailing blank lines belong to the gap before the next section,
            // not to this one, so the insertion goes above them.
            let kept = body
                .iter()
                .take(len)
                .rposition(|l| !matches!(l, Line::Blank(_)))
                .map_or(0, |last| last + 1);
            Some(i + 1 + kept)
        });

        if let Some(pos) = insert_at {
            self.lines.insert(
                pos,
                Line::Pair {
                    section: section_l,
                    key: key_l,
                    prefix: format!("{key} ="),
                    value: format!(" {value}"),
                },
            );
        } else {
            if !self.lines.is_empty() {
                self.lines.push(Line::Blank(String::new()));
            }
            self.lines.push(Line::Section {
                raw: format!("[{section_l}]"),
                name: section_l.clone(),
            });
            self.lines.push(Line::Pair {
                section: section_l,
                key: key_l,
                prefix: format!("{key} ="),
                value: format!(" {value}"),
            });
        }
    }

    /// The file as text, with CRLF line endings throughout and a trailing
    /// newline.
    ///
    /// Normalising the endings rather than preserving whatever the file had is
    /// a decision, not an oversight. This is a Windows-only utility whose INI
    /// is edited in Notepad and whose log is written CRLF for the same reason;
    /// carrying the file's dominant ending through a round trip would be
    /// machinery for a scenario — a diff in someone else's version control —
    /// that this utility does not have.
    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Blank(s) => out.push_str(s),
                Line::Section { raw, .. } => out.push_str(raw),
                Line::Pair { prefix, value, .. } => {
                    out.push_str(prefix);
                    out.push_str(value);
                }
            }
            out.push_str("\r\n");
        }
        out
    }
}

/// Splits an optional leading `+`/`-` sign off a value, returning the sign and
/// the remaining digits. A bare value keeps a positive sign.
fn split_sign(v: &str) -> (bool, &str) {
    if let Some(rest) = v.strip_prefix('-') {
        (false, rest)
    } else if let Some(rest) = v.strip_prefix('+') {
        (true, rest)
    } else {
        (true, v)
    }
}

/// Splits an optional `0x`/`0X` prefix off a value, returning the radix and the
/// remaining digits. Without the prefix the value is decimal.
fn split_radix(v: &str) -> (u32, &str) {
    v.strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .map_or((10, v), |hex| (16, hex))
}

/// Parses an unsigned integer with an optional `0x` prefix. Rejects a sign so
/// that a negative value is not silently wrapped into a large positive one.
fn parse_u32(v: &str) -> Option<u32> {
    let (positive, digits) = split_sign(v);
    if !positive {
        return None;
    }
    let (radix, digits) = split_radix(digits);
    u32::from_str_radix(digits, radix).ok()
}

/// Parses a signed integer with an optional `0x` prefix and optional sign, so
/// the two accessors share one notion of what an integer literal looks like.
fn parse_i32(v: &str) -> Option<i32> {
    let (positive, digits) = split_sign(v);
    let (radix, digits) = split_radix(digits);
    let magnitude = i64::from_str_radix(digits, radix).ok()?;
    let signed = if positive { magnitude } else { -magnitude };
    i32::try_from(signed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_unknown_keys_and_comments() {
        let src = "; header comment\n[general]\nlanguage = ru\ncustom = keepme\n";
        let mut ini = Ini::parse(src);
        ini.set("general", "language", "en");
        let out = ini.to_text();
        assert!(out.contains("custom = keepme"));
        assert!(out.contains("; header comment"));
        assert!(out.contains("language = en"));
        assert!(!out.contains("language = ru"));
    }

    #[test]
    fn set_creates_missing_section() {
        let mut ini = Ini::parse("[general]\na = 1\n");
        ini.set("window", "panel_x", "100");
        let out = ini.to_text();
        assert!(out.contains("[window]"));
        assert!(out.contains("panel_x = 100"));
        assert!(out.contains("a = 1"));
    }

    #[test]
    fn parses_hex_and_decimal() {
        let ini = Ini::parse("[device]\nvid = 0x0764\npid = 1537\n");
        assert_eq!(ini.get_u32("device", "vid"), Some(0x0764));
        assert_eq!(ini.get_u32("device", "pid"), Some(1537));
    }

    #[test]
    fn i32_understands_hex_like_u32() {
        let ini = Ini::parse("[t]\na = 0x10\nb = 0X20\nc = -5\nd = +7\n");
        assert_eq!(ini.get_i32("t", "a"), Some(16));
        assert_eq!(ini.get_i32("t", "b"), Some(32));
        assert_eq!(ini.get_i32("t", "c"), Some(-5));
        assert_eq!(ini.get_i32("t", "d"), Some(7));
    }

    #[test]
    fn u32_rejects_a_negative_value() {
        let ini = Ini::parse("[t]\na = -1\n");
        assert_eq!(ini.get_u32("t", "a"), None);
    }

    #[test]
    fn signed_hex_round_trips_through_i32() {
        let ini = Ini::parse("[t]\na = -0x1A\n");
        assert_eq!(ini.get_i32("t", "a"), Some(-26));
    }

    /// Matching is case-insensitive; writing back is not a licence to
    /// re-case. `[General]` must survive a `set` on another key untouched.
    #[test]
    fn section_header_case_survives_a_round_trip() {
        let mut ini = Ini::parse("[General]\ninterval = 3\n");
        ini.set("general", "language", "ru");
        let out = ini.to_text();
        assert!(
            out.contains("[General]"),
            "the user's casing was rewritten: {out}"
        );
        assert!(
            !out.contains("[general]"),
            "a duplicate section appeared: {out}"
        );
        assert_eq!(ini.get("GENERAL", "language"), Some("ru"));
    }

    /// Matching is case-insensitive; writing back is not a licence to
    /// re-case that either. `Language` and its indentation must survive a
    /// `set` on the same key — only the value is the caller's to replace.
    #[test]
    fn key_case_and_indent_survive_a_rewrite() {
        let mut ini = Ini::parse("[general]\n  Language = ru\n");
        ini.set("general", "language", "en");
        let out = ini.to_text();
        assert!(
            out.contains("  Language = en"),
            "the key was rewritten instead of the value: {out:?}"
        );
        assert_eq!(ini.get("general", "LANGUAGE"), Some("en"));
    }

    /// An inline comment is part of the value, and stays part of it.
    ///
    /// Nothing requires `;` to be honoured inside a value, and the numeric
    /// accessors reject what results, so a commented number falls back to its
    /// default rather than being half-read. Pinned so the behaviour is a
    /// decision rather than an accident: a future value parser that split on
    /// `;` would change what an existing file means.
    #[test]
    fn an_inline_comment_belongs_to_the_value() {
        let ini = Ini::parse("[t]\na = 5 ; five\n");
        assert_eq!(ini.get("t", "a"), Some("5 ; five"));
        assert_eq!(ini.get_u32("t", "a"), None);
    }

    /// A pair keeps the spacing it was written with, on both sides of the `=`.
    ///
    /// A file that is opened in an editor is a file whose shape belongs to
    /// whoever wrote it. A value written back with a space in front of it, into
    /// a line that had none, produced `a= 2` — spaced on one side and tight on
    /// the other, which reads as a typo rather than a convention. Keys this
    /// module appends still get one shape, `Key = value`; keys it finds keep
    /// theirs.
    #[test]
    fn a_pair_keeps_the_spacing_it_was_written_with() {
        let mut tight = Ini::parse("[t]\na=1\n");
        assert_eq!(tight.to_text(), "[t]\r\na=1\r\n");
        tight.set("t", "a", "2");
        assert_eq!(
            tight.to_text(),
            "[t]\r\na=2\r\n",
            "a tight pair must not come back spaced on one side of the `=`"
        );

        let mut spaced = Ini::parse("[t]\na = 1\n");
        spaced.set("t", "a", "2");
        assert_eq!(
            spaced.to_text(),
            "[t]\r\na = 2\r\n",
            "a spaced pair must keep its spacing"
        );
    }

    /// A new key lands at the end of the section it names.
    ///
    /// Reading the value back afterwards is true of a great many wrong
    /// implementations — inserting directly under the header, overwriting the
    /// neighbouring key, dropping the blank line before the next section — so
    /// this pins the *text* of the file instead. The file is edited by hand;
    /// where a line went and what happened to the ones around it is the whole
    /// observable behaviour of `set`, and a round trip cannot see any of it.
    #[test]
    fn a_new_key_goes_to_the_end_of_its_own_section() {
        let mut ini = Ini::parse("[general]\na = 1\nb = 2\n\n[window]\nx = 10\n");
        ini.set("general", "c", "3");
        assert_eq!(
            ini.to_text(),
            "[general]\r\na = 1\r\nb = 2\r\nc = 3\r\n\r\n[window]\r\nx = 10\r\n"
        );
    }

    /// The section searched for is the one named, not the first one in the
    /// file.
    ///
    /// Separate from the test above because a file whose only section is the
    /// target cannot tell the two apart: with one section, "the section named"
    /// and "wherever the scan first stops" are the same index. Here the target
    /// is second, so an insertion computed from the wrong header lands in
    /// `[general]` — a window coordinate written into the general section,
    /// which the next load then ignores in favour of the default.
    #[test]
    fn a_new_key_is_not_appended_to_the_wrong_section() {
        let mut ini = Ini::parse("[general]\na = 1\n\n[window]\nx = 10\n");
        ini.set("window", "y", "20");
        assert_eq!(
            ini.to_text(),
            "[general]\r\na = 1\r\n\r\n[window]\r\nx = 10\r\ny = 20\r\n"
        );
    }

    /// A comment closing a section stays below the key appended to it.
    ///
    /// A comment line is `Blank` to this parser — that is what keeps its text
    /// verbatim — and the trailing-run rule therefore treats it the way it
    /// treats an empty line: the new key goes above. The property worth
    /// pinning is the first half, not the placement: a comment that stopped
    /// being `Blank` would become a `Pair` the moment it contained an `=`,
    /// which every commented-out setting does, and `; language = ru` would
    /// then be a key named `; language` sitting in the section. The text of
    /// the file would be unchanged — prefix and value concatenate back to the
    /// original line — so nothing but the placement of the *next* appended key
    /// can see it happen.
    ///
    /// Both markers are checked because the parser accepts both and they are
    /// two separate conditions.
    #[test]
    fn a_commented_out_setting_is_a_comment_not_a_key() {
        let mut semicolon = Ini::parse("[general]\na = 1\n; language = ru\n");
        semicolon.set("general", "b", "2");
        assert_eq!(
            semicolon.to_text(),
            "[general]\r\na = 1\r\nb = 2\r\n; language = ru\r\n"
        );

        let mut hash = Ini::parse("[general]\na = 1\n# language = ru\n");
        hash.set("general", "b", "2");
        assert_eq!(
            hash.to_text(),
            "[general]\r\na = 1\r\nb = 2\r\n# language = ru\r\n"
        );
    }

    /// A section this call has to create is separated from what precedes it,
    /// and only when there is something to separate it from.
    ///
    /// The blank line is the difference between a file a person can read and
    /// one where `[window]` is welded to the last key of `[general]`. The
    /// empty-file half is what says the separator is conditional rather than
    /// unconditional: a fresh configuration must not open with a blank line.
    #[test]
    fn a_created_section_is_separated_only_when_something_precedes_it() {
        let mut existing = Ini::parse("[general]\na = 1\n");
        existing.set("window", "panel_x", "100");
        assert_eq!(
            existing.to_text(),
            "[general]\r\na = 1\r\n\r\n[window]\r\npanel_x = 100\r\n"
        );

        let mut empty = Ini::parse("");
        empty.set("general", "a", "1");
        assert_eq!(empty.to_text(), "[general]\r\na = 1\r\n");
    }

    #[test]
    fn tolerates_bom_and_case() {
        let ini = Ini::parse("\u{feff}[General]\nLanguage = ru\n");
        assert_eq!(ini.get("general", "language"), Some("ru"));
    }

    #[test]
    fn bool_forms() {
        let ini = Ini::parse("[n]\na = true\nb = 0\nc = YES\nd = junk\n");
        assert_eq!(ini.get_bool("n", "a"), Some(true));
        assert_eq!(ini.get_bool("n", "b"), Some(false));
        assert_eq!(ini.get_bool("n", "c"), Some(true));
        assert_eq!(ini.get_bool("n", "d"), None);
    }
}
